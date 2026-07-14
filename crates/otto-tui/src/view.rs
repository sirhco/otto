//! Pure rendering: `view(&App, &mut Frame)`.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::state::{
    App, COMMANDS, FilePickerState, LineCache, MentionState, Overlay, PaletteState, SearchState,
    TextInputState, TodoStatus, TranscriptItem, file_matches, palette_matches, ranked_matches,
};

/// Floor below which the layout is squeezed unusably tight — render a plain
/// message instead of fighting for space.
const MIN_COLS: u16 = 60;
const MIN_ROWS: u16 = 10;

/// Render the whole UI for the current `App`.
pub fn view(app: &App, frame: &mut Frame) {
    let area = frame.area();
    if area.width < MIN_COLS || area.height < MIN_ROWS {
        let msg = format!("terminal too small — need at least {MIN_COLS}×{MIN_ROWS}");
        frame.render_widget(
            Paragraph::new(msg)
                .style(app.theme.text_muted)
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }
    // Startup splash takes the whole frame until it dismisses. If it no longer
    // fits (terminal shrank mid-splash), fall through to the normal UI.
    if app.splash.is_some() {
        let variant = crate::splash::splash_variant(area.width, area.height);
        if variant != crate::splash::SplashVariant::None {
            crate::splash::render(frame, area, variant, &app.theme);
            return;
        }
    }
    let has_chip = !app.attachments.is_empty();
    let todos_h = todos_row_height(app, area.height);
    let busy = app.is_busy();
    let input_inner_w = area.width.saturating_sub(2 + PROMPT_W);
    let input_h = input_height(app, input_inner_w);
    let rows = Layout::vertical([
        Constraint::Length(1),                            // header
        Constraint::Min(1),                               // transcript
        Constraint::Length(todos_h),                      // todo panel
        Constraint::Length(if has_chip { 1 } else { 0 }), // attachment chip
        Constraint::Length(if busy { 1 } else { 0 }),     // activity line (busy-only)
        Constraint::Length(input_h),                      // input (grows to INPUT_CAP)
        Constraint::Length(2), // hints (wrapped: usage/ctx suffix can push past one row at narrow widths)
    ])
    .split(area);

    header(app, frame, rows[0]);
    let more_below = transcript(app, frame, rows[1]);
    if todos_h > 0 {
        todos_panel(frame, rows[2], app);
    }
    if has_chip {
        chip_line(app, frame, rows[3]);
    }
    if busy {
        activity_line(app, frame, rows[4]);
    }
    input(app, frame, rows[5]);
    hints(app, frame, rows[6], more_below);

    match &app.overlay {
        Overlay::None => {}
        Overlay::Help => overlay_text(frame, area, " Help ", HELP_FULL, &app.theme),
        Overlay::Permission(p) => {
            permission_overlay(frame, area, &p.permission, &p.patterns, &app.theme)
        }
        Overlay::Sessions => list_overlay(
            frame,
            area,
            " Sessions ",
            app.selected,
            &app.sessions
                .iter()
                .map(|s| s.title.clone().unwrap_or_else(|| s.id.clone()))
                .collect::<Vec<_>>(),
            &app.theme,
        ),
        Overlay::Models => list_overlay(
            frame,
            area,
            " Model ",
            app.selected,
            &app.models.iter().map(|m| m.id()).collect::<Vec<_>>(),
            &app.theme,
        ),
        Overlay::Agents => list_overlay(
            frame,
            area,
            " Agent ",
            app.selected,
            &app.agents
                .iter()
                .map(|a| a.name.clone())
                .collect::<Vec<_>>(),
            &app.theme,
        ),
        Overlay::Palette(ps) => palette_overlay(frame, area, ps, &app.theme),
        Overlay::TextInput(s) => text_input_overlay(frame, area, s, &app.theme),
        Overlay::Files(s) => files_overlay(frame, area, s, &app.attachments, &app.theme),
        // Anchored to the input row (`rows[5]`), floating above it; the editor
        // keeps the cursor (see `input()`'s focus predicate).
        Overlay::Mention(m) => mention_dropdown(
            frame,
            rows[5],
            m,
            &app.mention_query().unwrap_or_default(),
            &app.theme,
        ),
        // The toggle only opens this when `app.workflow.is_some()`, so an
        // unexpected `None` (e.g. a race) simply renders nothing.
        Overlay::WorkflowStatus => {
            if let Some(w) = &app.workflow {
                workflow_overlay(frame, area, w, &app.theme);
            }
        }
        // Search has no floating overlay widget of its own — it repurposes
        // the input row (`input()`) as a search bar and jump-scrolls the
        // transcript (`transcript()`); nothing extra to draw here.
        Overlay::Search(_) => {}
    }
}

/// Complete binding reference, shown behind the `?` Help overlay only.
const HELP_FULL: &str = "enter send · shift+enter newline · esc stop · ctrl+n new · ctrl+k cmds · ctrl+g agent · ↑↓ tool · ctrl+t toggle · ctrl+o todos · ctrl+y yank · / search · ? help · ctrl+c quit · ctrl+f attach · @ mention · shift+tab mode";

/// Slim footer hints, input empty: the still-bare keys (`/`, `?`) plus the
/// command hub, since the former letter shortcuts moved to ctrl chords.
const HINTS_EMPTY: &str = "enter send · ↑↓ select · / search · @ mention · ? help · ctrl+k cmds";

/// Slim footer hints, while typing: bare-letter commands stop firing once
/// there's text in the buffer, so only universally-live chords show.
const HINTS_TYPING: &str = "enter send · shift+enter newline · ctrl+k cmds";

/// Spinner animation frames, one glyph per `Msg::Tick`.
const SPIN: [char; 4] = ['⠋', '⠙', '⠹', '⠸'];
/// Tick rate driving the spinner/elapsed indicator. Must match the interval
/// the tick task in `lib.rs` sends `Msg::Tick` on (125ms = 8/s).
/// `pub(crate)`: state.rs derives the retry-backoff countdown from it too.
pub(crate) const TICKS_PER_SEC: u32 = 8;
/// Ticks between playful-word rotations while thinking (~5s at 8 ticks/s).
const ROTATE_TICKS: u32 = 40;

/// Format an elapsed-seconds count for the busy indicators: bare seconds under a
/// minute (`45s`), minutes + zero-padded seconds at or above one (`1m 05s`).
fn fmt_elapsed(secs: u32) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m {:02}s", secs / 60, secs % 60)
    }
}

/// A busy-only line directly above the input: what the agent is doing now.
/// Animated (spinner + elapsed) with a literal action while a tool runs, else a
/// playful word that rotates every `ROTATE_TICKS`. Callers gate on
/// `app.is_busy()` (the row is height 0 when idle), so this always renders.
fn activity_line(app: &App, frame: &mut Frame, area: Rect) {
    let t = &app.theme;
    let spin = SPIN[app.spinner_frame % SPIN.len()];
    let secs = app.busy_ticks / TICKS_PER_SEC;
    let body = match app.running_tool() {
        Some((name, title)) => crate::narration::tool_action(name, title),
        None => crate::narration::narration_word(app.busy_ticks / ROTATE_TICKS).to_string(),
    };
    let text = format!("{spin} {body}… ({})", fmt_elapsed(secs));
    let text = truncate_cols(&text, area.width as usize);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(text, t.accent))),
        area,
    );
}

fn header(app: &App, frame: &mut Frame, area: Rect) {
    let t = &app.theme;
    let session = app.session_id.as_deref().unwrap_or("(no session)");
    let model = app.model.as_deref().unwrap_or("default");
    let agent = app.agent.as_deref().unwrap_or("build");
    let mode = app.permission_mode.as_str();

    // Right-aligned status: a colored dot + word (glyph pairing survives mono
    // and CVD), spinner + elapsed while busy, red+bold on error.
    let status = app.status.trim();
    let is_error = status.starts_with("error");
    let is_busy = app.is_busy();
    let (dot_style, right) = if is_error {
        (t.status_err, format!("● {status} "))
    } else if is_busy {
        let spin = SPIN[app.spinner_frame % SPIN.len()];
        let secs = app.busy_ticks / TICKS_PER_SEC;
        (
            t.status_warn,
            format!("{spin} ● {status} {} ", fmt_elapsed(secs)),
        )
    } else if let Some(f) = &app.flash {
        (t.accent, format!("✓ {} ", f.msg))
    } else {
        (t.status_ok, format!("● {status} "))
    };

    use unicode_width::UnicodeWidthStr;
    let width = area.width as usize;
    let right_w = UnicodeWidthStr::width(right.as_str());
    // Degrade the meta at narrow widths so the status dot+word never clips:
    // drop trailing segments in order model → agent → session, keeping brand.
    let candidates = [
        format!(" otto · {session} · {agent} · {model} · mode:{mode} "),
        format!(" otto · {session} · {agent} · {model} "),
        format!(" otto · {session} · {agent} "),
        format!(" otto · {session} "),
        " otto ".to_string(),
    ];
    let left = candidates
        .iter()
        .find(|c| UnicodeWidthStr::width(c.as_str()) + right_w <= width)
        .cloned()
        .unwrap_or_else(|| " otto ".to_string());

    // `" otto"` is a 6-byte ASCII prefix — a byte split is safe even when the
    // session/model segments contain multibyte characters.
    let (brand, meta) = left.split_at(" otto".len());
    let pad = width.saturating_sub(UnicodeWidthStr::width(left.as_str()) + right_w);
    let mut spans = vec![Span::styled(brand.to_string(), t.accent)];
    // Color the `mode:<mode>` segment distinctly (when it survived the width
    // degradation) so the active permission mode is obvious — dim for the safe
    // default, escalating to bold red for full-auto.
    if let Some(idx) = meta.find("mode:") {
        let (meta_pre, mode_seg) = meta.split_at(idx);
        spans.push(Span::styled(meta_pre.to_string(), t.text_muted));
        spans.push(Span::styled(mode_seg.to_string(), mode_style(t, mode)));
    } else {
        spans.push(Span::styled(meta.to_string(), t.text_muted));
    }
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled(right, dot_style));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Style for the header's `mode:<mode>` segment. Escalates with how much the
/// mode auto-approves, so a full-auto session reads at a glance: dim for
/// approve-each (safe default), yellow for accept-edits, bold red for full-auto.
fn mode_style(t: &crate::theme::Theme, mode: &str) -> Style {
    use ratatui::style::Modifier;
    match mode {
        "full-auto" => t.status_err.add_modifier(Modifier::BOLD),
        "accept-edits" => t.status_warn,
        _ => t.text_muted,
    }
}

/// Render the transcript pane. Returns whether newer content exists below
/// the current scroll position (i.e. the user has scrolled up off the
/// bottom), so the caller can surface a "▼ more" hint.
fn transcript(app: &App, frame: &mut Frame, area: Rect) -> bool {
    let t = &app.theme;
    if app.transcript.is_empty() {
        // A quiet placeholder so a fresh session isn't a blank void.
        frame.render_widget(
            Paragraph::new("type a message · ctrl+k for commands")
                .style(t.text_muted)
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: true }),
            area,
        );
        return false;
    }
    // Line assembly (markdown parse + tool render) is expensive and
    // width-independent, so it's memoized in `app.line_cache`, keyed by
    // `app.render_gen` (bumped on transcript mutation, see state.rs) and the
    // render width. Rebuilt only when either changes — not every frame.
    let width = area.width;
    {
        let mut cache = app.line_cache.borrow_mut();
        let stale = match cache.as_ref() {
            Some(c) => c.r#gen != app.render_gen || c.width != width,
            None => true,
        };
        if stale {
            let prev = cache.take();
            *cache = Some(rebuild_line_cache(app, width, prev.as_ref()));
        }
    }
    let cache = app.line_cache.borrow();
    let cached = cache.as_ref().unwrap();
    // Work on a per-frame CLONE so highlight spans never accumulate in the
    // cached base (the cache is reused verbatim across frames until the next
    // invalidation).
    let mut lines = cached.lines.clone();
    let total = cached.wrap_total;
    // Publish the current scroll bound so `App::scroll_up` can clamp —
    // without this, holding PageUp builds invisible overscroll debt that
    // PageDown then silently unwinds with no visible movement.
    app.last_scroll_max.set(scroll_max(total, area.height));

    // While a search is active with at least one match, jump-scroll to
    // center the current match instead of following `app.scroll`, and
    // highlight the matched substrings.
    if let Overlay::Search(s) = &app.overlay {
        let matches = search_matches(&lines, &s.query);
        if !matches.is_empty() {
            let idx = s.current.min(matches.len() - 1);
            let current_line = matches[idx];
            highlight_search(&mut lines, &s.query, Some(current_line), t);
            // Map the LOGICAL match line to its wrapped ROW — using the
            // logical index as a row offset drifts as soon as anything wraps.
            let mi = cached
                .line_wrap_starts
                .get(current_line)
                .copied()
                .unwrap_or(0);
            let offset = search_offset(mi, total, area.height);
            render_scrolled(frame, area, lines, &cached.line_wrap_starts, offset);
            // Searching overrides the normal "▼ more" hint; keeping this
            // `false` is simplest (the search bar itself already shows
            // position via the `i/count` ordinal).
            return false;
        }
    }

    // A selected tool row (main focus) overlays an accent bar on its lines and
    // jump-scrolls to center it — mirroring the search overlay, on the same
    // per-frame clone so the cached base stays clean. Search and selection are
    // mutually exclusive (search is an overlay; selection is main-focus only).
    if let Some(sel) = app.tool_cursor {
        let starts = &cached.item_line_starts;
        if let Some(&start) = starts.get(sel) {
            let end = starts.get(sel + 1).copied().unwrap_or(lines.len());
            for line in lines.iter_mut().take(end).skip(start) {
                if let Some(first) = line.spans.first_mut() {
                    // Replace the leading two-space indent with an accent bar.
                    if first.content.starts_with("  ") {
                        let rest = first.content[2..].to_string();
                        *first = Span::styled(format!("▌ {rest}"), t.accent);
                    }
                }
            }
            // Logical item-start line → wrapped row (same mapping as search).
            let row = cached.line_wrap_starts.get(start).copied().unwrap_or(0);
            let offset = search_offset(row, total, area.height);
            render_scrolled(frame, area, lines, &cached.line_wrap_starts, offset);
            return offset < scroll_max(total, area.height);
        }
    }

    // `app.scroll` is wrapped-rows-from-bottom (0 = following newest content).
    let (offset, clamped_scroll) = scroll_offset(app.scroll, total, area.height);
    render_scrolled(frame, area, lines, &cached.line_wrap_starts, offset);
    // Show the indicator for ANY scrolled-up position, including fully at
    // the top (where `clamped_scroll == max`, so `clamped_scroll < max`
    // would wrongly go false right when the most content is hidden below).
    // "scrolled up at all" is exactly "from-bottom offset, clamped, is > 0".
    clamped_scroll > 0
}

/// Convenience wrapper over `transcript_lines_with_starts` that drops the
/// per-item start offsets. Used by `render_search_bar()` (which only needs
/// the line set to compute the live match count against `search_matches`'s
/// definition of "the transcript, right now") and by tests; `transcript()`
/// itself calls `transcript_lines_with_starts` directly since it also needs
/// the starts for jump-scrolling to a selected tool row.
fn transcript_lines(app: &App) -> Vec<Line<'static>> {
    transcript_lines_with_starts(app).0
}

/// Assemble transcript lines AND record where each item starts. Built in one
/// pass so `starts[i]` cannot drift from the actual line output.
fn transcript_lines_with_starts(app: &App) -> (Vec<Line<'static>>, Vec<usize>) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut starts: Vec<usize> = Vec::with_capacity(app.transcript.len());
    for item in &app.transcript {
        starts.push(lines.len());
        lines.extend(render_item(item, &app.theme));
    }
    (lines, starts)
}

/// Assemble ONE transcript item's lines (the per-item unit the incremental
/// [`rebuild_line_cache`] memoizes).
fn render_item(item: &TranscriptItem, t: &crate::theme::Theme) -> Vec<Line<'static>> {
    match item {
        TranscriptItem::User(text) => vec![Line::from(vec![
            Span::styled("› ", t.accent),
            Span::styled(text.clone(), t.text),
        ])],
        TranscriptItem::Assistant(text) => crate::render::markdown::render_markdown(text),
        TranscriptItem::Reasoning(text) => {
            vec![Line::from(Span::styled(format!("({text})"), t.reasoning))]
        }
        TranscriptItem::Error(msg) => vec![Line::from(vec![
            Span::styled("✖ error: ", t.status_err),
            Span::styled(msg.clone(), t.status_err),
        ])],
        TranscriptItem::Workflow(text) => {
            vec![Line::from(Span::styled(text.clone(), t.text_muted))]
        }
        TranscriptItem::Tool {
            name,
            status,
            title,
            input,
            output,
            expanded,
        } => crate::render::tool::render_tool(
            name, status, title, input, output, *expanded, false, t,
        ),
    }
}

/// Content hash of one transcript item — the reuse key for its memoized
/// render. A tool's `input` is set once at creation and never mutated, so it
/// is deliberately excluded (hashing a large JSON per rebuild would defeat
/// the memo); everything that CAN change (status, output, expanded) is in.
fn item_fingerprint(item: &TranscriptItem) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    std::mem::discriminant(item).hash(&mut h);
    match item {
        TranscriptItem::User(s)
        | TranscriptItem::Assistant(s)
        | TranscriptItem::Reasoning(s)
        | TranscriptItem::Error(s)
        | TranscriptItem::Workflow(s) => s.hash(&mut h),
        TranscriptItem::Tool {
            name,
            status,
            title,
            input: _,
            output,
            expanded,
        } => {
            name.hash(&mut h);
            format!("{status:?}").hash(&mut h);
            title.hash(&mut h);
            output.hash(&mut h);
            expanded.hash(&mut h);
        }
    }
    h.finish()
}

/// Rebuild the transcript [`LineCache`] incrementally: items whose fingerprint
/// (and the cache width) match the previous build reuse their rendered lines
/// and wrap count via `Arc`, so a streaming delta re-renders only the open
/// item instead of re-parsing the whole transcript — O(open item) per delta,
/// not O(history).
fn rebuild_line_cache(app: &App, width: u16, prev: Option<&LineCache>) -> LineCache {
    // A width change invalidates the per-item wrap counts, so only reuse
    // entries built at the same width.
    let prev_items: &[crate::state::ItemCacheEntry] = match prev {
        Some(c) if c.width == width => &c.items,
        _ => &[],
    };
    let mut items = Vec::with_capacity(app.transcript.len());
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut starts: Vec<usize> = Vec::with_capacity(app.transcript.len());
    let mut line_wrap_starts: Vec<u32> = Vec::new();
    let mut wrap_total: u32 = 0;
    for (i, item) in app.transcript.iter().enumerate() {
        let fingerprint = item_fingerprint(item);
        let entry = match prev_items.get(i) {
            Some(e) if e.fingerprint == fingerprint => e.clone(),
            _ => {
                let rendered = render_item(item, &app.theme);
                // Measure each line WRAPPED — `line_count` without `.wrap(...)`
                // returns the LOGICAL count, which undercounts the total,
                // shortens scroll_max, and makes the bottom of the transcript
                // unreachable (worse the narrower the terminal). Per-line
                // counts also feed `line_wrap_starts`, which maps logical line
                // indices to viewport rows for jump-scroll/search and the
                // sliced render.
                let line_wraps: Vec<u16> = rendered
                    .iter()
                    .map(|l| {
                        Paragraph::new(l.clone())
                            .wrap(Wrap { trim: false })
                            .line_count(width) as u16
                    })
                    .collect();
                crate::state::ItemCacheEntry {
                    fingerprint,
                    lines: std::sync::Arc::new(rendered),
                    line_wraps: std::sync::Arc::new(line_wraps),
                }
            }
        };
        starts.push(lines.len());
        for w in entry.line_wraps.iter() {
            line_wrap_starts.push(wrap_total);
            wrap_total += u32::from(*w);
        }
        lines.extend(entry.lines.iter().cloned());
        items.push(entry);
    }
    LineCache {
        r#gen: app.render_gen,
        width,
        lines,
        wrap_total,
        item_line_starts: starts,
        line_wrap_starts,
        items,
    }
}

/// Indices of `lines` whose concatenated span text contains `pattern`,
/// case-insensitive (ascii-lowercase on both sides). An empty pattern is
/// inert — it matches nothing, so an unentered search shows `0/0` rather
/// than "every line".
fn search_matches(lines: &[Line], pattern: &str) -> Vec<usize> {
    if pattern.is_empty() {
        return Vec::new();
    }
    let needle = pattern.to_ascii_lowercase();
    lines
        .iter()
        .enumerate()
        .filter_map(|(i, line)| {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            text.to_ascii_lowercase().contains(&needle).then_some(i)
        })
        .collect()
}

/// The top-of-viewport offset (in wrapped rows) that would show the very
/// bottom of a `total`-row paragraph in a `height`-row viewport. Shared by
/// `scroll_offset` (normal follow/scroll-back) and `search_offset`
/// (jump-to-match) so both clamp against the same bound.
fn scroll_max(total: u32, height: u16) -> u32 {
    total.saturating_sub(u32::from(height))
}

/// Convert `scroll` (wrapped rows from the bottom; 0 = following the newest
/// content) into a top-of-viewport row offset. `scroll == 0` maps to
/// `offset == max` (showing the bottom); larger `scroll` walks the offset
/// back toward the top, clamped so it can't underflow. Returns
/// `(offset, clamped_scroll)` — the latter lets the caller detect "scrolled
/// up at all".
fn scroll_offset(scroll: u32, total: u32, height: u16) -> (u32, u32) {
    let max = scroll_max(total, height);
    let clamped_scroll = scroll.min(max);
    (max.saturating_sub(clamped_scroll), clamped_scroll)
}

/// The top-of-viewport row offset that centers wrapped row `mi` (the current
/// search match / selected item start) within a `height`-row viewport over
/// `total` wrapped rows.
fn search_offset(mi: u32, total: u32, height: u16) -> u32 {
    let max = scroll_max(total, height);
    mi.saturating_sub(u32::from(height / 2)).min(max)
}

/// Render `lines` scrolled down by `offset` wrapped rows. `Paragraph::scroll`
/// takes a u16, so instead of passing a potentially huge offset, slice the
/// line list at the logical line containing the target row (via the
/// `line_wrap_starts` prefix sums) and hand the widget only the small
/// residual within that line.
fn render_scrolled(
    frame: &mut Frame,
    area: Rect,
    lines: Vec<Line<'static>>,
    line_wrap_starts: &[u32],
    offset: u32,
) {
    // Last logical line starting at or before the target row.
    let idx = line_wrap_starts
        .partition_point(|&s| s <= offset)
        .saturating_sub(1);
    let residual = offset.saturating_sub(line_wrap_starts.get(idx).copied().unwrap_or(0));
    let para = Paragraph::new(lines[idx.min(lines.len())..].to_vec()).wrap(Wrap { trim: false });
    frame.render_widget(para.scroll((residual.min(u32::from(u16::MAX)) as u16, 0)), area);
}

/// Restyle the matched substrings of an active search in place. For each line,
/// every span whose text contains `pattern` (case-insensitive, matching
/// `search_matches`'s ascii-lowercase rule) is split into before / matched /
/// after fragments; the matched fragment is overlaid (via `Style::patch`, so
/// the base fg/modifiers survive) with `theme.selection` when its line index
/// equals `current_line` (the match the viewport is centered on) and
/// `theme.search_match` otherwise. Every occurrence in a span is highlighted,
/// not just the first. All styling comes from theme tokens — no `Color::` here.
fn highlight_search(
    lines: &mut [Line<'_>],
    pattern: &str,
    current_line: Option<usize>,
    theme: &crate::theme::Theme,
) {
    if pattern.is_empty() {
        return;
    }
    let needle = pattern.to_ascii_lowercase();
    for (i, line) in lines.iter_mut().enumerate() {
        let hl = if Some(i) == current_line {
            theme.selection
        } else {
            theme.search_match
        };
        let mut out: Vec<Span> = Vec::new();
        for span in std::mem::take(&mut line.spans) {
            out.extend(split_span(span, &needle, hl));
        }
        line.spans = out;
    }
}

/// Split one span around every case-insensitive occurrence of `needle`
/// (already lowercased), overlaying `hl` on the matched fragments and keeping
/// the original span's style on the rest. Returns the span unchanged (as a
/// single-element vec) when it holds no match. `to_ascii_lowercase` preserves
/// byte offsets, so lowercased-haystack indices slice the original safely.
fn split_span<'a>(span: Span<'a>, needle: &str, hl: Style) -> Vec<Span<'a>> {
    let base = span.style;
    // An empty needle matches everywhere and would loop forever below; the sole
    // caller already guards it, but keep the fn self-safe.
    let hay = span.content.to_ascii_lowercase();
    if needle.is_empty() || !hay.contains(needle) {
        return vec![span];
    }
    let s = span.content;
    let mut out: Vec<Span> = Vec::new();
    let mut cur = 0usize;
    while cur < s.len() {
        match hay[cur..].find(needle) {
            Some(rel) => {
                let mstart = cur + rel;
                let mend = mstart + needle.len();
                if mstart > cur {
                    out.push(Span::styled(s[cur..mstart].to_string(), base));
                }
                out.push(Span::styled(s[mstart..mend].to_string(), base.patch(hl)));
                cur = mend;
            }
            None => {
                out.push(Span::styled(s[cur..].to_string(), base));
                break;
            }
        }
    }
    out
}

/// Max input-box height in rows (content + 2 borders); past this the box
/// scrolls internally.
const INPUT_CAP: u16 = 10;
/// Display width of the prompt glyph `▌ ` on the first visual row (and the
/// matching blank indent on continuation rows).
const PROMPT_W: u16 = 2;

/// Input-box height: wrapped visual rows plus top/bottom border, floored at 3
/// (one content row) and capped at `INPUT_CAP`.
fn input_height(app: &App, inner_width: u16) -> u16 {
    let rows = crate::input::wrap_rows(app.input.lines(), inner_width).len() as u16;
    (rows + 2).clamp(3, INPUT_CAP)
}

/// Stateless cursor-follow scroll offset: the smallest offset such that
/// `cursor_row` sits within a window of `visible` rows anchored at the bottom.
fn input_scroll(cursor_row: u16, visible: u16) -> u16 {
    cursor_row.saturating_sub(visible.saturating_sub(1))
}

fn input(app: &App, frame: &mut Frame, area: Rect) {
    if let Overlay::Search(s) = &app.overlay {
        render_search_bar(app, s, frame, area);
        return;
    }
    let t = &app.theme;
    // The input owns focus whenever no overlay is capturing keys. The inline
    // `@`-mention dropdown is the exception: the cursor stays live in the
    // editor while its floating list completes.
    let focused = matches!(app.overlay, Overlay::None | Overlay::Mention(_));
    let border_style = if focused { t.border_focus } else { t.border };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style);

    let inner_w = area.width.saturating_sub(2 + PROMPT_W);
    let rows = crate::input::wrap_rows(app.input.lines(), inner_w);
    let visible = area.height.saturating_sub(2); // content rows inside the box
    let (cur_vr, cur_col) = app.input.cursor_visual(inner_w);
    let offset = input_scroll(cur_vr, visible);
    let buf = app.input.lines();

    let lines: Vec<Line> = rows
        .iter()
        .skip(offset as usize)
        .take(visible as usize)
        .enumerate()
        .map(|(vi, w)| {
            let frag = &buf[w.logical_row][w.start..w.end];
            let prefix = if vi == 0 {
                Span::styled("▌ ", t.accent)
            } else {
                Span::styled("  ", t.text)
            };
            Line::from(vec![prefix, Span::styled(frag.to_string(), t.text)])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines).block(block), area);

    if focused {
        let screen_row = cur_vr.saturating_sub(offset);
        let max_x = area.x + area.width.saturating_sub(2);
        let x = (area.x + 1 + PROMPT_W + cur_col).min(max_x);
        let y = area.y + 1 + screen_row.min(area.height.saturating_sub(3));
        frame.set_cursor_position((x, y));
    }
}

/// Render the transcript-search bar in place of the editor row while
/// `Overlay::Search` is open: `▌ /{query}    {i}/{count}`, where `i` is the
/// 1-based current-match ordinal (`0/0` when the query is empty or has no
/// matches). Mirrors `input()`'s focused accent/text styling.
fn render_search_bar(app: &App, s: &SearchState, frame: &mut Frame, area: Rect) {
    let t = &app.theme;
    let lines = transcript_lines(app);
    let matches = search_matches(&lines, &s.query);
    let (i, count) = if matches.is_empty() {
        (0, 0)
    } else {
        (s.current.min(matches.len() - 1) + 1, matches.len())
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(t.border_focus);
    let line = Line::from(vec![
        Span::styled("▌ /", t.accent),
        Span::styled(s.query.clone(), t.text),
        Span::styled(format!("    {i}/{count}"), t.text_muted),
    ]);
    frame.render_widget(Paragraph::new(line).block(block), area);
}

/// Render the single-line attachment chip (`📎 name, name (N)`) shown above
/// the input whenever `App.attachments` is non-empty.
fn chip_line(app: &App, frame: &mut Frame, area: Rect) {
    frame.render_widget(
        Paragraph::new(attachment_chip(&app.attachments)).style(app.theme.text_muted),
        area,
    );
}

/// Basenames of the attached paths, joined for the chip line, e.g.
/// `📎 main.rs, Cargo.toml (2)`.
fn attachment_chip(attachments: &[String]) -> String {
    let names: Vec<&str> = attachments
        .iter()
        .map(|p| p.rsplit('/').next().unwrap_or(p))
        .collect();
    format!("📎 {} ({})", names.join(", "), names.len())
}

/// Height of the todo row: 0 hidden, 1 collapsed bar, else items + 2 borders capped to ~⅓ screen.
fn todos_row_height(app: &App, term_height: u16) -> u16 {
    if !app.todos_active() {
        return 0;
    }
    if app.todos_collapsed {
        return 1;
    }
    let cap = (term_height / 3).max(3);
    ((app.todos.len() as u16) + 2).min(cap)
}

fn truncate_cols(s: &str, cols: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    if UnicodeWidthStr::width(s) <= cols {
        return s.to_string();
    }
    // Reserve one column for the ellipsis.
    let budget = cols.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0usize;
    for ch in s.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > budget {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

/// Render the todo panel: a collapsed `▸ Todos n/m` bar, or a bordered list
/// of items with status glyphs (mirrors `files_overlay`/`palette_overlay`'s
/// bordered-block styling).
fn todos_panel(frame: &mut Frame, area: Rect, app: &App) {
    let theme = &app.theme;
    let (done, total) = app.todos_done_total();
    if app.todos_collapsed {
        frame.render_widget(
            Paragraph::new(Line::from(format!("▸ Todos {done}/{total}"))),
            area,
        );
        return;
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border)
        .title(format!(" Todos {done}/{total} "));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = inner.height as usize;
    let width = inner.width as usize;
    let overflow = app.todos.len() > rows;
    let visible = if overflow {
        rows.saturating_sub(1)
    } else {
        app.todos.len()
    };

    let mut lines: Vec<Line> = Vec::with_capacity(rows);
    for t in app.todos.iter().take(visible) {
        let (glyph, style) = match t.status {
            TodoStatus::Completed => ('✓', theme.text_muted),
            TodoStatus::InProgress => ('•', theme.status_warn),
            TodoStatus::Pending => (' ', theme.text),
            TodoStatus::Cancelled => ('✗', theme.text_muted),
        };
        let content = truncate_cols(&t.content, width.saturating_sub(2));
        lines.push(Line::from(Span::styled(
            format!("{glyph} {content}"),
            style,
        )));
    }
    if overflow {
        let k = app.todos.len() - visible;
        lines.push(Line::from(Span::styled(
            format!("  +{k} more"),
            theme.text_muted,
        )));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn hints(app: &App, frame: &mut Frame, area: Rect, more_below: bool) {
    let t = &app.theme;
    let slim = if app.input.is_empty() {
        HINTS_EMPTY
    } else {
        HINTS_TYPING
    };
    let mut base = if more_below {
        format!("▼ more · {slim}")
    } else {
        slim.to_string()
    };
    // While a workflow run is in flight, surface the cancel + panel chords.
    if app.workflow.as_ref().is_some_and(|w| w.done.is_none()) {
        base = format!("{base} · ctrl+x cancel · ctrl+w status");
    }
    if let Some(usage) = app.usage_line() {
        base = format!("{base}  ·  {usage}");
    }
    let mut spans = vec![Span::styled(base, t.text_muted)];
    if let Some(pct) = app.context_pct() {
        // Threshold: muted <70, warn 70–90, err >90.
        let style = if pct > 90 {
            t.status_err
        } else if pct >= 70 {
            t.status_warn
        } else {
            t.text_muted
        };
        spans.push(Span::styled(format!("  ·  {pct}% ctx"), style));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).wrap(Wrap { trim: true }),
        area,
    );
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn overlay_text(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    body: &str,
    theme: &crate::theme::Theme,
) {
    let r = centered(area, 50, 6);
    frame.render_widget(Clear, r);
    frame.render_widget(
        Paragraph::new(body).wrap(Wrap { trim: true }).block(
            Block::default()
                .title(title.to_string())
                .borders(Borders::ALL)
                .border_style(theme.border_focus),
        ),
        r,
    );
}

fn permission_overlay(
    frame: &mut Frame,
    area: Rect,
    permission: &str,
    patterns: &[String],
    theme: &crate::theme::Theme,
) {
    let body = format!(
        "allow `{permission}`? {}\n\ny once · a always · n reject",
        patterns.join(", ")
    );
    overlay_text(frame, area, " Permission ", &body, theme);
}

fn list_overlay(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    selected: usize,
    items: &[String],
    theme: &crate::theme::Theme,
) {
    let h = (items.len() as u16 + 2).max(3).min(area.height);
    let r = centered(area, 50, h);
    frame.render_widget(Clear, r);
    let lines: Vec<Line> = if items.is_empty() {
        vec![Line::from(Span::styled("(none)", theme.text_muted))]
    } else {
        items
            .iter()
            .enumerate()
            .map(|(i, it)| {
                if i == selected {
                    Line::from(Span::styled(format!("> {it}"), theme.selection))
                } else {
                    Line::from(format!("  {it}"))
                }
            })
            .collect()
    };
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title(title.to_string())
                .borders(Borders::ALL)
                .border_style(theme.border_focus),
        ),
        r,
    );
}

/// Render the ctrl+k command palette: a query line followed by the
/// fuzzy-filtered `COMMANDS` matching it (or `(no match)`), mirroring
/// `list_overlay`'s `Clear` + bordered-block + `theme.selection`-selected-row style.
fn palette_overlay(frame: &mut Frame, area: Rect, ps: &PaletteState, theme: &crate::theme::Theme) {
    let matches = palette_matches(&ps.query);
    let rows = matches.len().max(1); // at least the "(no match)" line
    let h = ((rows as u16) + 3).max(4).min(area.height); // query line + 2 borders
    let r = centered(area, 50, h);
    frame.render_widget(Clear, r);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border_focus)
        .title(" Commands ".to_string());
    let inner = block.inner(r);
    frame.render_widget(block, r);

    let mut lines: Vec<Line> = Vec::with_capacity(rows + 1);
    lines.push(Line::from(format!("> {}", ps.query)));
    if matches.is_empty() {
        lines.push(Line::from(Span::styled("(no match)", theme.text_muted)));
    } else {
        use unicode_width::UnicodeWidthStr;
        let inner_w = inner.width as usize;
        for (row, &ci) in matches.iter().enumerate() {
            let (label, key, _) = COMMANDS[ci];
            // 2-col row prefix ("> "/"  ") + 1 trailing space around the key.
            let pad = inner_w.saturating_sub(
                2 + UnicodeWidthStr::width(label) + UnicodeWidthStr::width(key) + 1,
            );
            if row == ps.selected {
                lines.push(Line::from(vec![
                    Span::styled(format!("> {label}"), theme.selection),
                    Span::styled(" ".repeat(pad), theme.selection),
                    Span::styled(format!("{key} "), theme.selection),
                ]));
            } else {
                lines.push(Line::from(vec![
                    Span::raw(format!("  {label}")),
                    Span::raw(" ".repeat(pad)),
                    Span::styled(format!("{key} "), theme.text_muted),
                ]));
            }
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Render the free-text input overlay (`Overlay::TextInput`): a centered box
/// titled with `s.title` showing the typed `s.query` and a block cursor,
/// mirroring `palette_overlay`'s `Clear` + bordered-block styling.
fn text_input_overlay(
    frame: &mut Frame,
    area: Rect,
    s: &TextInputState,
    theme: &crate::theme::Theme,
) {
    const MAX: usize = 6;
    // When a mention is active, show ≤6 ranked candidates (biased toward
    // `.otto/plans/`) between the input line and the hint.
    let cand: Vec<usize> = s
        .mention
        .as_ref()
        .filter(|m| !m.loading)
        .map(|m| {
            let ranked = ranked_matches(&m.results, &s.query[m.anchor + 1..], Some(".otto/plans/"));
            ranked.into_iter().take(MAX).collect()
        })
        .unwrap_or_default();
    let mention_loading = s.mention.as_ref().is_some_and(|m| m.loading);
    // Extra rows: loading / no-match placeholder (1) or the candidate list.
    let extra = if s.mention.is_some() {
        cand.len().max(1)
    } else {
        0
    };
    let h = (4 + extra as u16).min(area.height); // input + hint + 2 borders + candidates
    let r = centered(area, 60, h);
    frame.render_widget(Clear, r);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border_focus)
        .title(format!(" {} ", s.title));
    let inner = block.inner(r);
    frame.render_widget(block, r);

    let mut lines = vec![Line::from(vec![
        Span::raw(format!("> {}", s.query)),
        Span::styled("▌", theme.text_muted),
    ])];
    if let Some(m) = &s.mention {
        if mention_loading {
            lines.push(Line::from(Span::styled("  loading…", theme.text_muted)));
        } else if cand.is_empty() {
            lines.push(Line::from(Span::styled("  (no match)", theme.text_muted)));
        } else {
            for (row, &ci) in cand.iter().enumerate() {
                let path = &m.results[ci];
                let is_dir = path.ends_with('/');
                if row == m.selected {
                    lines.push(Line::from(Span::styled(
                        format!("> {path}"),
                        theme.selection,
                    )));
                } else {
                    let style = if is_dir { theme.text_muted } else { theme.text };
                    lines.push(Line::from(Span::styled(format!("  {path}"), style)));
                }
            }
        }
        lines.push(Line::from(Span::styled(
            "↑↓ pick · tab/enter insert · esc cancel",
            theme.text_muted,
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "enter start · esc cancel",
            theme.text_muted,
        )));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Render the ctrl+f file-attachment picker: a query line, a loading/empty
/// state, or the fuzzy-filtered `s.results` (via `file_matches`), each row
/// prefixed with `✓ ` when already in `attachments`, mirroring
/// `palette_overlay`'s `Clear` + bordered-block + `theme.selection`-selected-row style.
fn files_overlay(
    frame: &mut Frame,
    area: Rect,
    s: &FilePickerState,
    attachments: &[String],
    theme: &crate::theme::Theme,
) {
    let matches = file_matches(&s.results, &s.query);
    let rows = matches.len().max(1); // at least the "(no files)"/"(no match)" line
    let h = ((rows as u16) + 4).max(5).min(area.height); // query + status + 2 borders
    let rect = centered(area, 60, h);
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border_focus)
        .title(" Attach File ");
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let mut lines: Vec<Line> = Vec::with_capacity(rows + 2);
    lines.push(Line::from(format!("> {}", s.query)));
    if s.loading {
        lines.push(Line::from(Span::styled("loading…", theme.text_muted)));
    } else if matches.is_empty() {
        lines.push(Line::from(Span::styled(
            if s.results.is_empty() {
                "(no files)"
            } else {
                "(no match)"
            },
            theme.text_muted,
        )));
    } else {
        for (row, &ci) in matches.iter().enumerate() {
            let path = &s.results[ci];
            let checked = attachments.iter().any(|a| a == path);
            let marker = if checked { "✓ " } else { "  " };
            let label = format!("{marker}{path}");
            if row == s.selected {
                lines.push(Line::from(Span::styled(
                    format!("> {label}"),
                    theme.selection,
                )));
            } else {
                lines.push(Line::from(format!("  {label}")));
            }
        }
        if s.truncated {
            lines.push(Line::from(Span::styled(
                "  … list truncated",
                theme.text_muted,
            )));
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Render the inline `@`-mention dropdown: a `Clear`-ed bordered box floating
/// directly ABOVE the input row (its bottom edge meets `input_rect.y`), spanning
/// the full input width. Shows up to 8 ranked candidates in a scroll window that
/// keeps the selection visible; directory rows are muted (they already carry a
/// trailing `/`). Loading / no-match / truncated states get their own line.
fn mention_dropdown(
    frame: &mut Frame,
    input_rect: Rect,
    m: &MentionState,
    query: &str,
    theme: &crate::theme::Theme,
) {
    const MAX: usize = 8;
    let matches = ranked_matches(&m.results, query, None);
    let window = matches.len().min(MAX);
    let content_rows = if m.loading || matches.is_empty() {
        1
    } else {
        window + usize::from(m.truncated)
    };
    let h = (content_rows as u16) + 2; // + borders
    let y = input_rect.y.saturating_sub(h);
    let rect = Rect {
        x: input_rect.x,
        y,
        // If the box would overrun the top of the screen (tiny terminal), it is
        // clamped so its bottom still meets the input row.
        width: input_rect.width,
        height: input_rect.y - y,
    };
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border_focus)
        .title(" @ mention ");
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let mut lines: Vec<Line> = Vec::with_capacity(content_rows);
    if m.loading {
        lines.push(Line::from(Span::styled("loading…", theme.text_muted)));
    } else if matches.is_empty() {
        lines.push(Line::from(Span::styled("(no match)", theme.text_muted)));
    } else {
        // Scroll window: keep the selected row on screen.
        let offset = m.selected.saturating_sub(window.saturating_sub(1));
        for (row, &ci) in matches.iter().enumerate().skip(offset).take(window) {
            let path = &m.results[ci];
            let is_dir = path.ends_with('/');
            if row == m.selected {
                lines.push(Line::from(Span::styled(
                    format!("> {path}"),
                    theme.selection,
                )));
            } else {
                let style = if is_dir { theme.text_muted } else { theme.text };
                lines.push(Line::from(Span::styled(format!("  {path}"), style)));
            }
        }
        if m.truncated {
            lines.push(Line::from(Span::styled(
                "  … list truncated",
                theme.text_muted,
            )));
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Glyph + style for a workflow task's status string. Terminal success labels
/// (`DONE`, and PLAN's `VERIFIED`) are `✔`; terminal failure/halt labels
/// (`FAILED`/`BLOCKED`/`NEEDS_CONTEXT`) are `✖`; `DONE_WITH_CONCERNS` is a
/// terminal-with-caveats warn-colored `✔`; a bare `pending` is the dot. Every
/// other label is genuinely in-progress (`IMPLEMENTED`/`REVIEWING`/`FIXING`/
/// `VERIFYING`/`WRITE_TEST`/`VERIFY_RED`/`GREEN_IMPL`/`VERIFY_GREEN`/
/// `REGRESSION`/…) and gets the spinner-like `⟳`. Case-insensitive so server
/// casing can't break the match.
fn workflow_task_glyph(status: &str, theme: &crate::theme::Theme) -> (char, Style) {
    match status.trim().to_ascii_uppercase().as_str() {
        "DONE" | "VERIFIED" => ('✔', theme.status_ok),
        "FAILED" | "BLOCKED" | "NEEDS_CONTEXT" => ('✖', theme.status_err),
        "DONE_WITH_CONCERNS" => ('✔', theme.status_warn),
        "PENDING" | "" => ('·', theme.text_muted),
        _ => ('⟳', theme.status_warn),
    }
}

/// Render the read-only workflow status panel (`Overlay::WorkflowStatus`): a
/// centered bordered box titled from `kind`+`arg`+run-state, one line per task
/// (sorted by index via the `BTreeMap` iteration order) with a status glyph,
/// and an `[esc close]` hint. Mirrors `palette_overlay`'s `Clear` +
/// bordered-block styling; all colors come from theme tokens.
fn workflow_overlay(
    frame: &mut Frame,
    area: Rect,
    w: &crate::state::WorkflowView,
    theme: &crate::theme::Theme,
) {
    let state = match w.done {
        None => "running",
        Some(true) => "done",
        Some(false) => "failed",
    };
    // 2 borders + summary line + one line per task + hint line.
    let body_rows = 2 + w.tasks.len() as u16;
    let h = (body_rows + 2).max(4).min(area.height);
    let r = centered(area, 60, h);
    frame.render_widget(Clear, r);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border_focus)
        .title(" Workflow status ");
    let inner = block.inner(r);
    frame.render_widget(block, r);

    let width = inner.width as usize;
    let mut lines: Vec<Line> = Vec::with_capacity(w.tasks.len() + 2);
    lines.push(Line::from(Span::styled(
        format!("{} · {} · {state}", w.kind, w.arg),
        theme.text,
    )));
    for (i, (status, detail)) in &w.tasks {
        let (glyph, style) = workflow_task_glyph(status, theme);
        let body = if detail.trim().is_empty() {
            format!(" task {i}  {glyph} {status}")
        } else {
            format!(" task {i}  {glyph} {status} ({detail})")
        };
        lines.push(Line::from(Span::styled(truncate_cols(&body, width), style)));
    }
    lines.push(Line::from(Span::styled("[esc close]", theme.text_muted)));
    frame.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sse::PermissionAsked;
    use crate::state::{App, Overlay, TodoItem, TodoStatus, ToolStatus, TranscriptItem};
    use ratatui::{Terminal, backend::TestBackend};

    fn render(app: &App) -> String {
        // Wide enough that a long right-aligned header status (e.g. the
        // rate-limited retry message) isn't clipped by the header's
        // `saturating_sub` pad math — see `header_renders_retry_status_busy_not_error`.
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| view(app, f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        buf.content().iter().map(|c| c.symbol()).collect::<String>()
    }

    fn render_sized(app: &App, w: u16, h: u16) -> String {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| view(app, f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        buf.content().iter().map(|c| c.symbol()).collect::<String>()
    }

    #[test]
    fn too_small_shows_floor_message() {
        let app = App::new();
        let text = render_sized(&app, 40, 8);
        assert!(text.contains("too small"), "floor message shown: {text:?}");
    }

    #[test]
    fn at_min_size_renders_normal_ui() {
        let app = App::new();
        let text = render_sized(&app, 60, 10);
        assert!(
            text.contains("otto"),
            "normal header shown at min size: {text:?}"
        );
    }

    #[test]
    fn splash_full_shows_portrait_not_normal_ui() {
        let mut app = App::new();
        app.splash = Some(crate::splash::SPLASH_TICKS);
        // Generously sized so the full portrait always fits regardless of how
        // the art file is spaced (splash.rs unit-tests the size thresholds).
        let text = render_sized(&app, 200, 60);
        assert!(text.contains('@'), "llama portrait painted: {text:?}");
        assert!(
            !text.contains("otto"),
            "normal header suppressed under splash"
        );
    }

    #[test]
    fn no_splash_renders_normal_ui() {
        let app = App::new(); // splash None
        let text = render_sized(&app, 100, 40);
        assert!(text.contains("otto"), "normal header when no splash");
    }

    #[test]
    fn elapsed_formats_minutes_and_seconds() {
        assert_eq!(fmt_elapsed(0), "0s");
        assert_eq!(fmt_elapsed(45), "45s");
        assert_eq!(fmt_elapsed(59), "59s");
        assert_eq!(fmt_elapsed(60), "1m 00s");
        assert_eq!(fmt_elapsed(65), "1m 05s");
        assert_eq!(fmt_elapsed(125), "2m 05s");
        assert_eq!(fmt_elapsed(3600), "60m 00s");
    }

    #[test]
    fn error_item_renders_in_transcript() {
        let mut app = App::new();
        app.transcript.push(TranscriptItem::Error(
            "lost connection to otto server".into(),
        ));
        let text = render(&app);
        assert!(text.contains("error:"), "error row painted: {text:?}");
        assert!(text.contains("lost connection to otto server"));
    }

    /// One column below the width floor still shows the floor message —
    /// pins the `<` (not `<=`) comparison on the width axis.
    #[test]
    fn one_col_below_floor_shows_message() {
        let app = App::new();
        let text = render_sized(&app, MIN_COLS - 1, MIN_ROWS);
        assert!(
            text.contains("too small"),
            "floor message shown one column below width floor: {text:?}"
        );
    }

    /// One row below the height floor still shows the floor message —
    /// pins the `<` (not `<=`) comparison on the height axis.
    #[test]
    fn one_row_below_floor_shows_message() {
        let app = App::new();
        let text = render_sized(&app, MIN_COLS, MIN_ROWS - 1);
        assert!(
            text.contains("too small"),
            "floor message shown one row below height floor: {text:?}"
        );
    }

    #[test]
    fn empty_session_shows_hint() {
        let app = App::new();
        let text = render(&app);
        assert!(text.contains("help"), "footer hint present: {text:?}");
    }

    #[test]
    fn transcript_shows_assistant_text() {
        let mut app = App::new();
        app.transcript
            .push(TranscriptItem::Assistant("hello world".into()));
        assert!(render(&app).contains("hello world"));
    }

    /// The transcript renders with `Wrap {trim:false}`, so the cache's line
    /// counts must be measured the same way. Measured unwrapped, a long line
    /// counts as 1 instead of its wrapped rows, `scroll_max` comes up short,
    /// and the bottom of the transcript is unreachable. This test fails when
    /// the measuring `Paragraph` loses its `.wrap(...)`.
    #[test]
    fn wrap_total_counts_wrapped_rows_not_logical_lines() {
        let mut app = App::new();
        // One unbroken ~600-char line: 1 logical line, many wrapped rows at
        // width 100.
        app.transcript
            .push(TranscriptItem::Assistant("x".repeat(600)));
        let _ = render(&app);
        let c = app.line_cache.borrow();
        let c = c.as_ref().unwrap();
        assert!(
            c.wrap_total >= 6,
            "600 chars at width ~100 must count >= 6 wrapped rows, got {}",
            c.wrap_total
        );
    }

    /// `line_wrap_starts` must map logical line indices to WRAPPED rows —
    /// jump-scroll and search center on these; with logical indices they
    /// drift as soon as any earlier line wraps.
    #[test]
    fn line_wrap_starts_account_for_earlier_wrapping() {
        let mut app = App::new();
        app.transcript
            .push(TranscriptItem::Assistant("x".repeat(600))); // wraps ~6+ rows
        app.transcript
            .push(TranscriptItem::Assistant("TARGET".into()));
        let _ = render(&app);
        let c = app.line_cache.borrow();
        let c = c.as_ref().unwrap();
        let target_logical = c.item_line_starts[1];
        let target_row = c.line_wrap_starts[target_logical];
        assert!(
            target_row > target_logical as u32,
            "wrapped row ({target_row}) must exceed the logical index \
             ({target_logical}) when an earlier line wraps"
        );
        assert_eq!(
            c.line_wrap_starts.len(),
            c.lines.len(),
            "one start per assembled line"
        );
        assert!(
            c.wrap_total >= target_row,
            "total covers every line's rows"
        );
    }

    /// A render must publish the scroll bound so `App::scroll_up` clamps
    /// against real geometry.
    #[test]
    fn render_publishes_scroll_max() {
        let mut app = App::new();
        for i in 0..80 {
            app.transcript
                .push(TranscriptItem::Assistant(format!("line {i}")));
        }
        let _ = render(&app);
        assert!(
            app.last_scroll_max.get() > 0,
            "80 items in a 20-row viewport must publish a positive scroll max"
        );
    }

    #[test]
    fn line_cache_reused_when_gen_and_width_match() {
        let mut app = App::new();
        app.transcript
            .push(TranscriptItem::Assistant("hello".into()));
        let _ = render(&app); // first render fills the cache
        let gen_before = app.line_cache.borrow().as_ref().unwrap().r#gen;
        let _ = render(&app); // second render, nothing changed
        let c = app.line_cache.borrow();
        let c = c.as_ref().unwrap();
        assert_eq!(c.r#gen, gen_before);
        assert_eq!(c.r#gen, app.render_gen); // cache tracks app gen
    }

    /// A streaming delta mutates only the open item; every completed item's
    /// rendered lines must be REUSED (same `Arc`), not re-parsed — re-rendering
    /// the whole transcript per delta is O(history) per delta and shows up as
    /// growing pauses in long sessions.
    #[test]
    fn line_cache_reuses_unchanged_item_renders_across_rebuilds() {
        let mut app = App::new();
        app.transcript
            .push(TranscriptItem::Assistant("# stable heading\nbody".into()));
        app.transcript
            .push(TranscriptItem::Assistant("streaming".into()));
        app.bump_render_for_test();
        let _ = render(&app); // fills the cache
        let stable_before =
            std::sync::Arc::clone(&app.line_cache.borrow().as_ref().unwrap().items[0].lines);
        let open_before =
            std::sync::Arc::clone(&app.line_cache.borrow().as_ref().unwrap().items[1].lines);

        // A delta lands on the open (second) item only.
        if let Some(TranscriptItem::Assistant(s)) = app.transcript.get_mut(1) {
            s.push_str(" more text");
        }
        app.bump_render_for_test();
        let _ = render(&app); // rebuild

        let cache = app.line_cache.borrow();
        let items = &cache.as_ref().unwrap().items;
        assert!(
            std::sync::Arc::ptr_eq(&stable_before, &items[0].lines),
            "unchanged item was re-rendered instead of reused"
        );
        assert!(
            !std::sync::Arc::ptr_eq(&open_before, &items[1].lines),
            "changed item must be re-rendered"
        );
    }

    #[test]
    fn line_cache_rebuilds_after_gen_bump() {
        let mut app = App::new();
        app.transcript
            .push(TranscriptItem::Assistant("hello".into()));
        let _ = render(&app);
        app.transcript.push(TranscriptItem::User("more".into()));
        app.bump_render_for_test();
        let _ = render(&app);
        assert_eq!(
            app.line_cache.borrow().as_ref().unwrap().r#gen,
            app.render_gen
        );
    }

    /// Highlighting must be applied to a per-frame clone, never the cached
    /// base — otherwise repeat highlighting would keep re-splitting the
    /// cached spans (each pass slicing the previous pass's fragments further),
    /// so a bug here shows up as the cached base's span count silently
    /// growing across cache-hit re-renders, even though the *displayed* text
    /// looks identical either way.
    #[test]
    fn highlight_does_not_mutate_cached_base_across_frames() {
        let mut app = App::new();
        app.transcript
            .push(TranscriptItem::Assistant("foo bar foo".into()));
        app.open_search();
        for c in "foo".chars() {
            app.search_input(c);
        }
        let _ = render(&app); // fills the cache, highlights a clone
        let _ = render(&app); // cache hit: same gen + width
        let cached_spans: usize = app
            .line_cache
            .borrow()
            .as_ref()
            .unwrap()
            .lines
            .iter()
            .map(|l| l.spans.len())
            .sum();
        let fresh_spans: usize = transcript_lines(&app).iter().map(|l| l.spans.len()).sum();
        assert_eq!(
            cached_spans, fresh_spans,
            "cached base must stay unhighlighted (span count stable, not growing)"
        );

        // Behavior-level companion check: the displayed match ordinal (one
        // matching line, "foo bar foo") must also be stable across renders.
        let first = render(&app);
        let second = render(&app);
        assert!(first.contains("1/1"), "expected a single match: {first:?}");
        assert_eq!(first.contains("1/1"), second.contains("1/1"));
    }

    #[test]
    fn header_renders_thinking_status() {
        let mut app = App::new();
        app.status = "…thinking".into();
        let text = render(&app);
        assert!(text.contains("thinking"), "busy status visible: {text:?}");
    }

    #[test]
    fn header_renders_error_status() {
        let mut app = App::new();
        app.status = "error: no anthropic credentials".into();
        assert!(
            render(&app).contains("error:"),
            "error status visible in header"
        );
    }

    #[test]
    fn header_renders_retry_status_busy_not_error() {
        let mut app = App::new();
        app.fold_event(otto_events::LLMEvent::Retry {
            attempt: 3,
            max: 5,
            delay_ms: 16000,
            salvaged: false,
            message: "http error: status 429".into(),
        });
        let text = render(&app);
        assert!(
            text.contains("rate-limited — retrying 3/5 (16s)"),
            "header shows retry status: {text:?}"
        );
    }

    /// The status dot color tracks state: green ready / yellow busy / red error.
    #[test]
    fn header_status_dot_color_by_state() {
        use ratatui::style::Color;
        fn dot_fg(app: &App) -> Option<Color> {
            let backend = TestBackend::new(100, 20);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|f| view(app, f)).unwrap();
            let buf = terminal.backend().buffer().clone();
            // Find the first '●' cell on the header row (y = 0).
            (0..buf.area.width)
                .map(|x| &buf[(x, 0)])
                .find(|c| c.symbol() == "●")
                .and_then(|c| c.style().fg)
        }
        let mut ready = App::new();
        ready.status = "ready".into();
        assert_eq!(dot_fg(&ready), Some(Color::Green));

        let mut busy = App::new();
        busy.status = "…thinking".into();
        assert_eq!(dot_fg(&busy), Some(Color::Yellow));

        let mut err = App::new();
        err.status = "error: boom".into();
        assert_eq!(dot_fg(&err), Some(Color::Red));
    }

    /// The `mode:` segment is colored by mode so full-auto stands out.
    #[test]
    fn header_mode_colored_by_mode() {
        use ratatui::style::Color;
        fn mode_fg(mode: &str) -> Option<Color> {
            let mut app = App::new();
            app.permission_mode = mode.into();
            let backend = TestBackend::new(120, 20);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|f| view(&app, f)).unwrap();
            let buf = terminal.backend().buffer().clone();
            let syms: Vec<String> = (0..buf.area.width)
                .map(|x| buf[(x, 0)].symbol().to_string())
                .collect();
            // Locate the "mode:" label on the header row by cell (byte offsets
            // don't map to cells — the meta contains multibyte `·` separators).
            let start = (0..syms.len()).find(|&i| {
                syms[i..]
                    .iter()
                    .take(5)
                    .map(String::as_str)
                    .collect::<String>()
                    == "mode:"
            })?;
            buf[(start as u16, 0)].style().fg
        }
        // full-auto → red (auto-approve on), accept-edits → yellow.
        assert_eq!(mode_fg("full-auto"), Some(Color::Red));
        assert_eq!(mode_fg("accept-edits"), Some(Color::Yellow));
    }

    /// At a narrow width the status dot + word survive (meta degrades first).
    #[test]
    fn header_keeps_status_at_narrow_width() {
        let mut app = App::new();
        app.status = "ready".into();
        app.session_id = Some("a-very-long-session-identifier".into());
        app.model = Some("anthropic/claude-sonnet-4-5".into());
        // Narrow enough to exercise the header's own degrade logic, but at or
        // above the floor guard's minimum (60x10) so `view()` doesn't just
        // render the "terminal too small" message instead. The full-degrade-
        // to-bare-brand case (model, session, *and* agent all dropped) relied
        // on widths now permanently below the floor guard, so that coverage
        // is intentionally retired rather than resurrected here.
        let backend = TestBackend::new(MIN_COLS, MIN_ROWS);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| view(&app, f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains('●'), "dot survives narrow: {text:?}");
        assert!(
            text.contains("ready"),
            "status word survives narrow: {text:?}"
        );
        assert!(text.contains("otto"), "brand survives narrow: {text:?}");
    }

    #[test]
    fn overlay_titles_are_title_case() {
        let mut app = App::new();
        app.overlay = Overlay::Help;
        assert!(render(&app).contains("Help"), "help title normalized");
    }

    #[test]
    fn permission_overlay_shows_prompt() {
        let mut app = App::new();
        app.overlay = Overlay::Permission(PermissionAsked {
            id: "p".into(),
            session_id: "s".into(),
            permission: "edit".into(),
            patterns: vec![],
        });
        let text = render(&app);
        assert!(text.contains("edit"), "shows permission name");
        assert!(
            text.contains('y') && text.contains('n'),
            "shows y/n choices"
        );
    }

    #[test]
    fn tool_item_renders_status_marker() {
        let mut app = App::new();
        app.transcript.push(TranscriptItem::Tool {
            name: "read".into(),
            status: ToolStatus::Ok,
            title: "read a.rs".into(),
            input: None,
            output: None,
            expanded: false,
        });
        let text = render(&app);
        assert!(text.contains("read a.rs"));
        assert!(text.contains('✓'), "shows OK status marker: {text:?}");
    }

    #[test]
    fn transcript_autofollows_to_newest() {
        let mut app = App::new();
        for i in 0..40 {
            app.transcript
                .push(TranscriptItem::Assistant(format!("item-{i}")));
        }

        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| view(&app, f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();

        assert!(
            text.contains("item-39"),
            "newest item should be visible: {text:?}"
        );
        assert!(
            !text.contains("item-0"),
            "earliest item should have scrolled out of view: {text:?}"
        );
    }

    #[test]
    fn more_indicator_shows_when_scrolled_to_top() {
        let mut app = App::new();
        for i in 0..40 {
            app.transcript
                .push(TranscriptItem::Assistant(format!("item-{i}")));
        }
        // Scroll far past the top; `transcript()` clamps this to `max`, the
        // offset that shows the very first line — the "▼ more" indicator
        // must still be visible here, since all the newer content below is
        // hidden. (Regression: it used to disappear exactly at this point.)
        app.scroll = 100;
        let text = render(&app);
        assert!(
            text.contains("▼ more"),
            "more-below indicator must show when scrolled to the top: {text:?}"
        );
    }

    #[test]
    fn assistant_markdown_and_autofollow() {
        let mut app = App::new();
        // scroll == 0 (default) == following the bottom.
        for i in 0..40 {
            app.transcript
                .push(TranscriptItem::Assistant(format!("**line {i}**")));
        }
        let text = render(&app); // render() helper renders at 100x20
        assert!(text.contains("line 39"), "newest visible");
        assert!(!text.contains("line 0"), "oldest scrolled off");
    }

    /// At the smallest terminal `view()` will actually render an overlay at
    /// (the floor, 60x10 — one row below just shows the floor message
    /// instead), the Models list must still fit into the cramped inner
    /// height without panicking. Several entries ensure the list actually
    /// has to squeeze rather than trivially fitting.
    #[test]
    fn list_overlay_does_not_panic_on_short_terminal() {
        let mut app = App::new();
        app.models = vec![
            crate::client::ModelChoice {
                provider: "anthropic".into(),
                model: "claude-3".into(),
            },
            crate::client::ModelChoice {
                provider: "anthropic".into(),
                model: "claude-3-opus".into(),
            },
            crate::client::ModelChoice {
                provider: "openai".into(),
                model: "gpt-4".into(),
            },
            crate::client::ModelChoice {
                provider: "openai".into(),
                model: "gpt-4-turbo".into(),
            },
            crate::client::ModelChoice {
                provider: "ollama".into(),
                model: "llama3".into(),
            },
        ];
        app.open_picker(Overlay::Models);

        let backend = TestBackend::new(MIN_COLS, MIN_ROWS);
        let mut terminal = Terminal::new(backend).unwrap();
        let result = terminal.draw(|f| view(&app, f));
        assert!(result.is_ok(), "draw must not panic on short terminal");
    }

    #[test]
    fn models_overlay_shows_items_and_selection_marker() {
        let mut app = App::new();
        app.models = vec![
            crate::client::ModelChoice {
                provider: "anthropic".into(),
                model: "claude-3".into(),
            },
            crate::client::ModelChoice {
                provider: "openai".into(),
                model: "gpt-4".into(),
            },
        ];
        app.open_picker(Overlay::Models);
        let text = render(&app);
        assert!(text.contains("anthropic/claude-3"), "{text:?}");
        assert!(text.contains("openai/gpt-4"), "{text:?}");
        assert!(text.contains('>'), "shows selection marker: {text:?}");
    }

    #[test]
    fn palette_overlay_shows_query_and_commands() {
        let mut app = App::new();
        app.open_palette();
        for c in "ch".chars() {
            app.palette_input(c);
        }
        let text = render(&app);
        assert!(text.contains("> ch"), "shows query line");
        assert!(text.contains("Change model"), "shows a filtered command");
        // "Change model…" ranks first for "ch" (tied word-start/consecutive
        // score with "Change agent…", broken by registry order in
        // `palette_matches`), so `selected == 0` marks its row with `> `.
        assert!(
            text.contains("> Change model"),
            "selected row shows > marker: {text:?}"
        );
    }

    #[test]
    fn palette_overlay_shows_no_match() {
        let mut app = App::new();
        app.open_palette();
        for c in "zzzz".chars() {
            app.palette_input(c);
        }
        let text = render(&app);
        assert!(text.contains("(no match)"));
    }

    #[test]
    fn files_overlay_shows_results_and_check_marker() {
        let mut app = App::new();
        app.open_file_picker();
        app.files_loaded(vec!["Cargo.toml".into(), "src/main.rs".into()], false);
        app.file_toggle(); // attach Cargo.toml (selected=0)
        let text = render(&app);
        assert!(text.contains("Cargo.toml"));
        assert!(text.contains('✓'), "attached row marked");
    }

    #[test]
    fn files_overlay_loading_and_no_match() {
        let mut app = App::new();
        app.open_file_picker();
        let loading = render(&app);
        assert!(loading.contains("loading"), "shows loading before results");
        app.files_loaded(vec!["a.rs".into()], false);
        for c in "zzz".chars() {
            app.file_input(c);
        }
        let text = render(&app);
        assert!(text.contains("(no match)"));
    }

    #[test]
    fn workflow_overlay_renders_tasks_and_state() {
        use crate::state::WorkflowView;
        let mut app = App::new();
        let mut tasks = std::collections::BTreeMap::new();
        tasks.insert(1u32, ("DONE".to_string(), "review clean".to_string()));
        tasks.insert(2u32, ("REVIEWING".to_string(), String::new()));
        tasks.insert(3u32, ("pending".to_string(), String::new()));
        app.workflow = Some(WorkflowView {
            kind: "sdd".into(),
            arg: "docs/plan.md".into(),
            session: "ses_1".into(),
            tasks,
            done: None,
        });
        app.overlay = Overlay::WorkflowStatus;
        let text = render(&app);
        assert!(text.contains("Workflow status"), "title: {text:?}");
        assert!(
            text.contains("sdd · docs/plan.md · running"),
            "summary: {text:?}"
        );
        assert!(text.contains("task 1"), "task row: {text:?}");
        assert!(text.contains('✔') && text.contains('⟳'), "glyphs: {text:?}");
        assert!(text.contains("esc close"), "hint: {text:?}");
    }

    #[test]
    fn footer_shows_cancel_hint_while_workflow_active() {
        use crate::state::WorkflowView;
        let mut app = App::new();
        app.workflow = Some(WorkflowView {
            kind: "sdd".into(),
            arg: "p.md".into(),
            session: "s".into(),
            tasks: Default::default(),
            done: None,
        });
        assert!(
            render(&app).contains("ctrl+x cancel"),
            "cancel hint shows while running"
        );
        // Once the run finishes, the hint drops.
        if let Some(w) = &mut app.workflow {
            w.done = Some(true);
        }
        assert!(
            !render(&app).contains("ctrl+x cancel"),
            "hint gone when done"
        );
    }

    #[test]
    fn chip_line_shows_attached_names() {
        let mut app = App::new();
        app.attachments = vec!["src/main.rs".into(), "Cargo.toml".into()];
        let text = render(&app);
        assert!(
            text.contains("main.rs") && text.contains("Cargo.toml"),
            "chip shows basenames"
        );
    }

    #[test]
    fn todos_panel_expanded_shows_count_and_glyphs() {
        let mut app = App::new();
        app.todos = vec![
            TodoItem {
                content: "scaffold crate".into(),
                status: TodoStatus::Completed,
            },
            TodoItem {
                content: "wire the loop".into(),
                status: TodoStatus::InProgress,
            },
            TodoItem {
                content: "write tests".into(),
                status: TodoStatus::Pending,
            },
        ];
        let text = render(&app);
        assert!(text.contains("Todos 1/3"), "shows done/total count");
        assert!(text.contains("scaffold crate"));
        assert!(
            text.contains('✓') && text.contains('•'),
            "status glyphs present"
        );
    }

    #[test]
    fn todos_panel_collapsed_shows_bar() {
        let mut app = App::new();
        app.todos = vec![TodoItem {
            content: "a".into(),
            status: TodoStatus::InProgress,
        }];
        app.todos_collapsed = true;
        let text = render(&app);
        assert!(text.contains("▸ Todos 0/1"), "collapsed bar with count");
        assert!(!text.contains("write tests"));
    }

    #[test]
    fn todos_panel_hidden_when_all_done() {
        let mut app = App::new();
        app.todos = vec![TodoItem {
            content: "done".into(),
            status: TodoStatus::Completed,
        }];
        let text = render(&app);
        assert!(!text.contains("Todos"), "no panel when nothing active");
    }

    #[test]
    fn empty_transcript_shows_placeholder() {
        let app = App::new();
        let text = render(&app);
        assert!(
            text.contains("type a message"),
            "empty-state hint: {text:?}"
        );
    }

    #[test]
    fn user_prefix_is_accent() {
        use ratatui::style::Color;
        let mut app = App::new();
        app.transcript.push(TranscriptItem::User("hi".into()));
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| view(&app, f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        // Find the '›' cell; it must be cyan.
        let fg = buf
            .content()
            .iter()
            .find(|c| c.symbol() == "›")
            .and_then(|c| c.style().fg);
        assert_eq!(fg, Some(Color::Cyan));
    }

    #[test]
    fn input_border_is_accent_when_focused_idle_when_overlay_open() {
        use ratatui::style::Color;
        fn border_fg(app: &App) -> Option<Color> {
            let backend = TestBackend::new(100, 20);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|f| view(app, f)).unwrap();
            let buf = terminal.backend().buffer().clone();
            // The input box is the bottom-most bordered region; scan rows
            // bottom-up for its top-left '┌' so an open overlay's own border
            // (which gains a color in a later task) can't shadow it.
            for y in (0..buf.area.height).rev() {
                for x in 0..buf.area.width {
                    if buf[(x, y)].symbol() == "┌" {
                        return buf[(x, y)].style().fg;
                    }
                }
            }
            None
        }
        let app = App::new(); // Overlay::None → focused
        assert_eq!(border_fg(&app), Some(Color::Cyan));

        let mut overlaid = App::new();
        overlaid.overlay = Overlay::Help;
        // Idle border is unstyled → the buffer keeps its default `Reset` fg
        // (ratatui cells default to `Reset`, not `None`), never the accent.
        assert_eq!(
            border_fg(&overlaid),
            Some(Color::Reset),
            "idle input border is unstyled, not accent"
        );
    }

    #[test]
    fn ctx_pct_color_crosses_thresholds() {
        use ratatui::style::Color;
        fn ctx_fg(pct_input: u64) -> Option<Color> {
            let mut app = App::new();
            app.model = Some("anthropic/claude-sonnet-4-5".into()); // 200k ctx
            app.fold_event(otto_events::LLMEvent::Finish {
                reason: otto_events::FinishReason::Stop,
                usage: Some(otto_events::Usage {
                    input_tokens: Some(pct_input),
                    output_tokens: Some(0),
                    total_tokens: Some(pct_input),
                    ..Default::default()
                }),
                provider_metadata: None,
            });
            let backend = TestBackend::new(200, 20);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|f| view(&app, f)).unwrap();
            let buf = terminal.backend().buffer().clone();
            // The hints widget occupies the bottom 2 rows; at this test
            // width the whole line fits on the first of them, so scan both
            // rather than assuming the very last row.
            (buf.area.height - 2..buf.area.height)
                .flat_map(|y| (0..buf.area.width).map(move |x| (x, y)))
                .map(|(x, y)| &buf[(x, y)])
                .find(|c| c.symbol() == "%")
                .and_then(|c| c.style().fg)
        }
        // Unstyled (muted) cells keep the buffer's default `Reset` fg, not
        // `None` — see `input_border_is_accent_when_focused_idle_when_overlay_open`.
        assert_eq!(ctx_fg(20_000), Some(Color::Reset)); // ~10% → muted (no fg)
        assert_eq!(ctx_fg(160_000), Some(Color::Yellow)); // 80% → warn
        assert_eq!(ctx_fg(196_000), Some(Color::Red)); // 98% → err
    }

    #[test]
    fn input_shows_prompt_glyph() {
        let app = App::new();
        assert!(render(&app).contains('▌'), "prompt glyph present");
    }

    #[test]
    fn mono_render_has_no_color() {
        let mut app = App::new();
        app.theme = crate::theme::Theme::mono();
        app.status = "ready".into();
        app.transcript.push(TranscriptItem::User("hi".into()));
        app.transcript.push(TranscriptItem::Tool {
            name: "read".into(),
            status: ToolStatus::Ok,
            title: "read a.rs".into(),
            input: None,
            output: None,
            expanded: false,
        });
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| view(&app, f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        // Mono guarantee: no cell carries a *chromatic* color. Unstyled cells
        // keep ratatui's default `Color::Reset` (never `None`), so the check is
        // that every fg/bg is `Reset`/`None` — never Cyan/Green/Red/Yellow/Rgb.
        use ratatui::style::Color;
        for c in buf.content() {
            let fg = c.style().fg;
            assert!(
                matches!(fg, None | Some(Color::Reset)),
                "mono cell {:?} has chromatic fg {fg:?}",
                c.symbol()
            );
            let bg = c.style().bg;
            assert!(
                matches!(bg, None | Some(Color::Reset)),
                "mono cell {:?} has chromatic bg {bg:?}",
                c.symbol()
            );
        }
        // Still legible: the status dot + glyph survive as text.
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(
            text.contains('●') && text.contains('✓'),
            "glyphs survive mono"
        );
    }

    #[test]
    fn renders_at_floor_sizes_without_panic() {
        for (w, h) in [(80, 24), (60, 24)] {
            let mut app = App::new();
            app.status = "ready".into();
            app.transcript.push(TranscriptItem::User("hello".into()));
            let backend = TestBackend::new(w, h);
            let mut terminal = Terminal::new(backend).unwrap();
            let r = terminal.draw(|f| view(&app, f));
            assert!(r.is_ok(), "must render at {w}x{h}");
            let buf = terminal.backend().buffer().clone();
            let text: String = buf.content().iter().map(|c| c.symbol()).collect();
            assert!(
                text.contains("otto") && text.contains('●'),
                "brand+dot at {w}x{h}"
            );
        }
    }

    /// The footer hints slim down to a context-aware subset: the full
    /// bare-letter reference only makes sense while those bindings are live
    /// (input empty). While typing, only universally-live chords show.
    #[test]
    fn hints_show_empty_set_when_input_empty() {
        let app = App::new();
        assert!(app.input.is_empty());
        let text = render(&app);
        assert!(text.contains("? help"), "empty-set token present: {text:?}");
        assert!(
            !text.contains("shift+enter newline"),
            "typing-only chord absent while empty: {text:?}"
        );
    }

    #[test]
    fn hints_show_typing_set_when_input_nonempty() {
        let mut app = App::new();
        app.input.insert('h');
        let text = render(&app);
        assert!(
            text.contains("shift+enter newline"),
            "typing-set chord present: {text:?}"
        );
        assert!(
            !text.contains("q quit"),
            "empty-set-only token absent while typing: {text:?}"
        );
    }

    #[test]
    fn search_matches_finds_case_insensitive_indices() {
        let lines = vec![
            Line::from("hello world"),
            Line::from("nothing here"),
            Line::from("HELLO again"),
        ];
        assert_eq!(search_matches(&lines, "hello"), vec![0, 2]);
        assert_eq!(search_matches(&lines, "HELLO"), vec![0, 2]);
        assert_eq!(search_matches(&lines, "nope"), Vec::<usize>::new());
    }

    #[test]
    fn search_matches_empty_pattern_is_inert() {
        let lines = vec![Line::from("anything"), Line::from("")];
        assert_eq!(search_matches(&lines, ""), Vec::<usize>::new());
    }

    #[test]
    fn search_matches_concatenates_multi_span_lines() {
        // A line built from several styled spans (as the transcript renderer
        // produces, e.g. the "› " prefix span + text span) must still match
        // against its full concatenated text, not just the first span.
        let lines = vec![Line::from(vec![Span::raw("› "), Span::raw("needle here")])];
        assert_eq!(search_matches(&lines, "needle"), vec![0]);
    }

    #[test]
    fn scroll_offset_matches_prior_scroll_math() {
        // total=50, height=10 -> max=40; scroll=0 follows the bottom (offset
        // == max); larger scroll walks back toward the top; over-scrolling
        // clamps at 0 rather than underflowing.
        assert_eq!(scroll_offset(0, 50, 10), (40, 0));
        assert_eq!(scroll_offset(5, 50, 10), (35, 5));
        assert_eq!(scroll_offset(1000, 50, 10), (0, 40));
    }

    #[test]
    fn search_offset_centers_match_and_clamps() {
        // total=100, height=20 -> centering line 50 puts it at offset 40.
        assert_eq!(search_offset(50, 100, 20), 40);
        // A match near the top clamps at 0 rather than going negative.
        assert_eq!(search_offset(2, 100, 20), 0);
        // A match near the bottom clamps at `max` (total - height = 80)
        // rather than overshooting past the last page.
        assert_eq!(search_offset(99, 100, 20), 80);
    }

    #[test]
    fn highlight_search_splits_all_occurrences() {
        // "foo bar foo" with pattern "foo" splits into before/match/after,
        // catching BOTH occurrences, not just the first.
        let mut lines = vec![Line::from("foo bar foo")];
        let theme = crate::theme::Theme::dark();
        highlight_search(&mut lines, "foo", Some(0), &theme);
        let parts: Vec<&str> = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(parts, vec!["foo", " bar ", "foo"]);
        // The two matched fragments carry the current-match token
        // (`selection`, REVERSED); the gap keeps the base (unstyled) style.
        use ratatui::style::Modifier;
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        assert!(
            !lines[0].spans[1]
                .style
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        assert!(
            lines[0].spans[2]
                .style
                .add_modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn highlight_search_is_case_insensitive() {
        let mut lines = vec![Line::from("Foo")];
        let theme = crate::theme::Theme::dark();
        highlight_search(&mut lines, "foo", Some(0), &theme);
        let parts: Vec<&str> = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        // Original casing preserved in the split fragment, matched regardless.
        assert_eq!(parts, vec!["Foo"]);
        use ratatui::style::Modifier;
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn highlight_search_current_vs_other_styles_differ() {
        // The match on `current_line` uses `selection`; matches on other lines
        // use `search_match`. The two must be visually distinguishable.
        let mut lines = vec![Line::from("hit"), Line::from("hit")];
        let theme = crate::theme::Theme::dark();
        highlight_search(&mut lines, "hit", Some(0), &theme);
        use ratatui::style::Modifier;
        let current = lines[0].spans[0].style;
        let other = lines[1].spans[0].style;
        assert!(current.add_modifier.contains(Modifier::REVERSED)); // selection
        assert!(other.add_modifier.contains(Modifier::UNDERLINED)); // search_match
        assert_ne!(current, other);
    }

    #[test]
    fn highlight_search_handles_multibyte_without_panic() {
        // The transcript renderer emits multibyte glyphs (›, ●, …) and
        // assistant text can be any UTF-8. `to_ascii_lowercase` leaves
        // multibyte bytes untouched, so lowercased-haystack byte offsets stay
        // on char boundaries in the original — slicing must not panic and must
        // land around the match cleanly.
        let mut lines = vec![Line::from("café ▸ CAFÉ ☃ foo")];
        let theme = crate::theme::Theme::dark();
        highlight_search(&mut lines, "foo", Some(0), &theme);
        let parts: Vec<&str> = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(parts, vec!["café ▸ CAFÉ ☃ ", "foo"]);

        // A match adjacent to multibyte content, and a multibyte needle, both
        // slice on valid boundaries.
        let mut lines = vec![Line::from("x café y")];
        highlight_search(&mut lines, "café", Some(0), &theme);
        let parts: Vec<&str> = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(parts, vec!["x ", "café", " y"]);
    }

    #[test]
    fn highlight_search_mono_has_no_color_but_keeps_modifier() {
        // Under NO_COLOR the matched fragment carries no chromatic fg but stays
        // visible via a modifier (underline for the non-current `search_match`).
        let mut lines = vec![Line::from("hit")];
        let theme = crate::theme::Theme::mono();
        highlight_search(&mut lines, "hit", None, &theme);
        use ratatui::style::Modifier;
        let style = lines[0].spans[0].style;
        assert_eq!(style.fg, None);
        assert!(style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn search_bar_shows_zero_of_zero_with_empty_query() {
        let mut app = App::new();
        app.open_search();
        let text = render(&app);
        assert!(text.contains("0/0"), "empty query shows 0/0: {text:?}");
    }

    #[test]
    fn search_bar_shows_match_count_and_ordinal() {
        let mut app = App::new();
        app.transcript
            .push(TranscriptItem::Assistant("foo bar".into()));
        app.transcript.push(TranscriptItem::Assistant("baz".into()));
        app.transcript
            .push(TranscriptItem::Assistant("another foo".into()));
        app.open_search();
        for c in "foo".chars() {
            app.search_input(c);
        }
        let text = render(&app);
        assert!(text.contains("/foo"), "shows typed query: {text:?}");
        assert!(text.contains("1/2"), "shows ordinal/count: {text:?}");
    }

    #[test]
    fn search_jump_scroll_brings_match_into_view() {
        let mut app = App::new();
        for i in 0..60 {
            app.transcript
                .push(TranscriptItem::Assistant(format!("item-{i}")));
        }
        // Following the bottom by default, "item-0" is scrolled far out of
        // view — confirm the baseline before searching.
        let baseline = render(&app);
        assert!(
            !baseline.contains("item-0"),
            "baseline: item-0 should be off-screen: {baseline:?}"
        );
        app.open_search();
        for c in "item-0".chars() {
            app.search_input(c);
        }
        let text = render(&app);
        assert!(
            text.contains("item-0"),
            "jump-scroll must bring the current match into view: {text:?}"
        );
    }

    #[test]
    fn truncate_cols_counts_display_width() {
        use super::truncate_cols;
        // Two 2-column CJK glyphs = 4 display columns; budget 3 must not overflow.
        let out = truncate_cols("你好", 3);
        assert!(
            unicode_width::UnicodeWidthStr::width(out.as_str()) <= 3,
            "{out:?}"
        );
        // ASCII unchanged within budget.
        assert_eq!(truncate_cols("abcd", 10), "abcd");
        // ASCII truncates with ellipsis as before.
        assert_eq!(truncate_cols("abcdef", 4), "abc…");
    }

    #[test]
    fn header_pads_by_display_width_not_char_count() {
        // A 2-column session name and an ASCII name of equal DISPLAY width must
        // right-align the status dot at the same column (no clip/misalign).
        let mut wide = App::new();
        wide.session_id = Some("字".into()); // 1 char, 2 columns
        wide.status = "ready".into();
        let text = render(&wide); // 100x20
        // The status dot must be present (not clipped) on the header row.
        let header_row: String = text.chars().take(100).collect();
        assert!(header_row.contains('●'), "dot present: {header_row:?}");
    }

    /// The `?` Help overlay is the one place the complete binding reference
    /// must survive the footer slim-down.
    #[test]
    fn help_overlay_contains_all_original_bindings() {
        let mut app = App::new();
        app.overlay = Overlay::Help;
        let text = render(&app);
        // Space-free chord tokens: line-wrapping in the help box can split a
        // multi-word label across rows, but a chord like "ctrl+t" never splits.
        // (Tail tokens like shift+tab can be clipped at this small test box
        // size; they still render in a real terminal.)
        for token in ["esc", "ctrl+n", "ctrl+g", "ctrl+t", "ctrl+o", "ctrl+f"] {
            assert!(
                text.contains(token),
                "help overlay missing {token:?}: {text:?}"
            );
        }
    }

    #[test]
    fn palette_overlay_shows_key_hints() {
        use crate::state::PaletteState;
        use ratatui::{Terminal, backend::TestBackend};
        let theme = crate::theme::Theme::dark();
        let ps = PaletteState {
            query: String::new(),
            selected: 0,
        };
        let mut term = Terminal::new(TestBackend::new(50, 14)).unwrap();
        term.draw(|f| palette_overlay(f, f.area(), &ps, &theme))
            .unwrap();
        let buf = term.backend().buffer().clone();
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();
        // A known key hint appears somewhere in the rendered palette.
        assert!(
            text.contains("ctrl+n"),
            "expected key hint in palette, got: {text}"
        );
        assert!(text.contains("New session"));
    }

    #[test]
    fn palette_key_hints_match_real_bindings() {
        // Every non-empty, non-workflow key_hint in COMMANDS must be a real
        // chord that appears in HELP_FULL — guards against the hint drifting
        // out of sync with input.rs's actual dispatch table again.
        let help_chords: Vec<&str> = HELP_FULL
            .split(" · ")
            .filter_map(|seg| seg.split(' ').next())
            .collect();
        for &(label, key_hint, _) in crate::state::COMMANDS {
            if label.starts_with("Workflow:") {
                continue; // prose, not a real key — intentionally excluded
            }
            if key_hint.is_empty() {
                continue; // palette-only command, correctly has no key
            }
            assert!(
                help_chords.contains(&key_hint),
                "COMMANDS key_hint {key_hint:?} for {label:?} not found in HELP_FULL \
                 (chords: {help_chords:?}) — binding likely moved without updating COMMANDS"
            );
        }
    }

    #[test]
    fn header_shows_flash_when_idle() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = crate::state::App::new();
        app.status = "ready".into();
        app.flash("copied");
        let mut term = Terminal::new(TestBackend::new(80, 3)).unwrap();
        term.draw(|f| header(&app, f, f.area())).unwrap();
        let text: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            text.contains("copied"),
            "flash should show in header, got: {text}"
        );
    }

    #[test]
    fn header_busy_suppresses_flash() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = crate::state::App::new();
        app.status = "…thinking".into(); // is_busy() == true
        app.flash("copied");
        let mut term = Terminal::new(TestBackend::new(80, 3)).unwrap();
        term.draw(|f| header(&app, f, f.area())).unwrap();
        let text: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(!text.contains("copied"), "busy state must win over flash");
        assert!(text.contains("thinking"));
    }

    #[test]
    fn input_sets_cursor_when_focused() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = crate::state::App::new();
        app.input.insert('h');
        app.input.insert('i');
        let mut term = Terminal::new(TestBackend::new(40, 3)).unwrap();
        // area is the full 40x3; input box border at (0,0), content row y=1.
        term.draw(|f| input(&app, f, f.area())).unwrap();
        // "▌ " is 2 cells + "hi" is 2 cells → cursor col = border(1)+2+2 = 5, row = 1.
        let pos = term.get_cursor_position().unwrap();
        assert_eq!((pos.x, pos.y), (5, 1));
    }

    #[test]
    fn input_cursor_clamps_to_content_row_after_newline() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = crate::state::App::new();
        app.input.insert('a');
        app.input.newline(); // editor row = 1, but the box has one content row
        let mut term = Terminal::new(TestBackend::new(40, 3)).unwrap();
        term.draw(|f| input(&app, f, f.area())).unwrap();
        let pos = term.get_cursor_position().unwrap();
        // Must stay on the content row (y = area.y + 1 = 1), never the bottom border (y = 2).
        assert_eq!(
            pos.y, 1,
            "cursor must clamp to the single content row, not the border"
        );
    }

    #[test]
    fn input_box_grows_for_two_lines() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = App::new();
        app.input.insert('a');
        app.input.newline();
        app.input.insert('b');
        // Width must be >= MIN_COLS (60) or `view` renders the "too small"
        // floor message instead of the real UI (see `too_small_shows_floor_message`).
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| view(&app, f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        // Locate the input box's border, mirroring
        // `input_border_is_accent_when_focused_idle_when_overlay_open`: it's
        // the bottom-most bordered region, so scan bottom-up for its
        // top-left '┌' and then down for the matching '└'.
        let (bx, top) = (0..buf.area.height)
            .rev()
            .find_map(|y| {
                (0..buf.area.width)
                    .find(|&x| buf[(x, y)].symbol() == "┌")
                    .map(|x| (x, y))
            })
            .expect("input box top border found");
        let bottom = (top + 1..buf.area.height)
            .find(|&y| buf[(bx, y)].symbol() == "└")
            .expect("input box bottom border found");
        // Merely checking the concatenated buffer text for 'a' and 'b' is not
        // enough here: unrelated chrome (header/hints) can also contain those
        // letters, and the pre-fix single-`Line` render embeds a literal '\n'
        // inside one row's text (never a real row break) so both letters
        // still show up in that one row regardless. Instead confirm the box
        // itself grew past the old fixed 3 rows, and that each logical line
        // landed on its own content row inside it.
        assert!(
            bottom - top + 1 > 3,
            "input box must grow past the old fixed height for 2 lines"
        );
        let row_of = |ch: char| {
            (top + 1..bottom)
                .find(|&y| (0..buf.area.width).any(|x| buf[(x, y)].symbol() == ch.to_string()))
        };
        let a_row = row_of('a').expect("'a' rendered inside the input box");
        let b_row = row_of('b').expect("'b' rendered inside the input box");
        assert_ne!(
            a_row, b_row,
            "each logical line must get its own visual row"
        );
    }

    #[test]
    fn input_wraps_long_line_across_rows() {
        let mut app = App::new();
        // Width must be >= MIN_COLS (60), so the wrapped tail has to be long
        // enough to overflow the resulting inner width of 56
        // (60 - 2 border - 2 prompt), unlike the original 16-col-inner design.
        let long: String = "0123456789".repeat(6).chars().take(56).collect::<String>() + "ghij";
        for c in long.chars() {
            app.input.insert(c);
        }
        let text = render_sized(&app, 60, 12);
        // Tail chars that only fit on a wrapped second row must still render.
        assert!(text.contains("ghij"));
    }

    #[test]
    fn activity_line_shows_running_tool() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = crate::state::App::new();
        app.status = "…thinking".into(); // busy
        app.transcript.push(crate::state::TranscriptItem::Tool {
            name: "bash".into(),
            status: crate::state::ToolStatus::Running,
            title: "bash ls -F".into(),
            input: None,
            output: None,
            expanded: false,
        });
        let mut term = Terminal::new(TestBackend::new(40, 1)).unwrap();
        term.draw(|f| activity_line(&app, f, f.area())).unwrap();
        let text: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        // Literal action, leading tool-name token stripped, animated spinner glyph.
        assert!(text.contains("Running ls -F"), "got: {text}");
        assert!(
            SPIN.iter().any(|g| text.contains(*g)),
            "spinner glyph present: {text}"
        );
    }

    #[test]
    fn activity_line_falls_back_to_playful_word() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = crate::state::App::new();
        app.status = "…thinking".into(); // busy, no running tool
        let mut term = Terminal::new(TestBackend::new(40, 1)).unwrap();
        term.draw(|f| activity_line(&app, f, f.area())).unwrap();
        let text: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        // A playful word (not the raw status) + spinner + elapsed.
        assert!(
            crate::narration::WORDS.iter().any(|w| text.contains(*w)),
            "playful word: {text}"
        );
        assert!(
            SPIN.iter().any(|g| text.contains(*g)),
            "spinner glyph present: {text}"
        );
        assert!(text.contains("(0s)"), "elapsed shown: {text}");
    }

    #[test]
    fn activity_line_word_rotates_over_time() {
        use ratatui::{Terminal, backend::TestBackend};
        let render_at = |ticks: u32| -> String {
            let mut app = crate::state::App::new();
            app.status = "…thinking".into();
            app.busy_ticks = ticks;
            let mut term = Terminal::new(TestBackend::new(40, 1)).unwrap();
            term.draw(|f| activity_line(&app, f, f.area())).unwrap();
            term.backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect()
        };
        // Across the first 8 rotation windows the rendered word must change at
        // least once (never looks frozen on a long think).
        let distinct: std::collections::HashSet<String> =
            (0..8u32).map(|r| render_at(r * ROTATE_TICKS)).collect();
        assert!(distinct.len() > 1, "activity word never rotated");
    }

    #[test]
    fn item_line_starts_track_each_item() {
        let mut app = crate::state::App::new();
        app.transcript
            .push(crate::state::TranscriptItem::User("one".into())); // 1 line
        app.transcript
            .push(crate::state::TranscriptItem::User("two".into())); // 1 line
        app.transcript
            .push(crate::state::TranscriptItem::Reasoning("r".into())); // 1 line
        let (lines, starts) = transcript_lines_with_starts(&app);
        assert_eq!(starts.len(), 3);
        assert_eq!(starts[0], 0);
        assert_eq!(starts[1], 1);
        assert_eq!(starts[2], 2);
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn selected_tool_row_gets_accent_bar_in_transcript() {
        use ratatui::style::Color;
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = crate::state::App::new();
        app.transcript.push(crate::state::TranscriptItem::Tool {
            name: "read".into(),
            status: crate::state::ToolStatus::Ok,
            title: "read a.rs".into(),
            input: None,
            output: None,
            expanded: false,
        });
        app.tool_cursor = Some(0);
        let mut term = Terminal::new(TestBackend::new(40, 6)).unwrap();
        term.draw(|f| {
            let _ = transcript(&app, f, f.area());
        })
        .unwrap();
        let buf = term.backend().buffer();
        // The tool row is the last content row; scan for a cyan '▌' cell.
        let has_accent_bar = buf
            .content()
            .iter()
            .any(|c| c.symbol() == "▌" && c.style().fg == Some(Color::Cyan));
        assert!(has_accent_bar, "selected tool row should show a cyan ▌ bar");
    }

    #[test]
    fn input_height_empty_is_three() {
        let app = App::new();
        assert_eq!(input_height(&app, 20), 3);
    }

    #[test]
    fn input_height_grows_with_lines() {
        let mut app = App::new();
        app.input.insert('a');
        app.input.newline();
        app.input.insert('b'); // 2 logical rows
        assert_eq!(input_height(&app, 20), 4); // 2 + 2 borders
    }

    #[test]
    fn input_height_caps_at_ten() {
        let mut app = App::new();
        for _ in 0..20 {
            app.input.insert('x');
            app.input.newline();
        }
        assert_eq!(input_height(&app, 20), INPUT_CAP);
    }

    #[test]
    fn input_scroll_keeps_cursor_visible() {
        // visible window of 8 rows.
        assert_eq!(input_scroll(0, 8), 0);
        assert_eq!(input_scroll(7, 8), 0);
        assert_eq!(input_scroll(8, 8), 1);
        assert_eq!(input_scroll(9, 8), 2);
    }

    #[test]
    fn input_prompt_glyph_visible_when_scrolled() {
        let mut app = App::new();
        // 12 logical lines -> input box caps at height 10 (8 visible content
        // rows) and scrolls, so offset > 0.
        for _ in 0..12 {
            app.input.insert('x');
            app.input.newline();
        }
        let text = render_sized(&app, 60, 24);
        // The accent prompt "▌" must still render on the first visible row.
        // (Empty transcript => input prompt is the only '▌' source.)
        assert!(
            text.contains('▌'),
            "prompt glyph must remain visible when the input box scrolls: {text:?}"
        );
    }

    #[test]
    fn workflow_task_glyph_maps_terminal_and_progress_labels() {
        let theme = crate::theme::Theme::default();
        // Terminal success: DONE and PLAN's VERIFIED both check off.
        assert_eq!(workflow_task_glyph("DONE", &theme).0, '✔');
        assert_eq!(workflow_task_glyph("VERIFIED", &theme).0, '✔');
        // Case-insensitive.
        assert_eq!(workflow_task_glyph("verified", &theme).0, '✔');
        // Terminal failure/halt: cross.
        assert_eq!(workflow_task_glyph("FAILED", &theme).0, '✖');
        assert_eq!(workflow_task_glyph("BLOCKED", &theme).0, '✖');
        assert_eq!(workflow_task_glyph("NEEDS_CONTEXT", &theme).0, '✖');
        // Terminal-with-caveats: a warn-colored check, not a spinner.
        assert_eq!(
            workflow_task_glyph("DONE_WITH_CONCERNS", &theme),
            ('✔', theme.status_warn)
        );
        // Pending dot.
        assert_eq!(workflow_task_glyph("PENDING", &theme).0, '·');
        // Genuinely in-progress labels keep the spinner.
        assert_eq!(workflow_task_glyph("REVIEWING", &theme).0, '⟳');
        assert_eq!(workflow_task_glyph("IMPLEMENTED", &theme).0, '⟳');
        assert_eq!(workflow_task_glyph("SOMETHING_UNKNOWN", &theme).0, '⟳');
    }

    // ----- Task D: inline `@` file/folder mention -------------------------

    #[test]
    fn mention_dropdown_renders_above_input_with_selection() {
        let mut app = App::new();
        app.input.insert('@');
        app.open_mention();
        app.files_loaded(vec!["src/main.rs".into(), "lib.rs".into()], false);
        let text = render(&app);
        assert!(text.contains("@ mention"), "dropdown title shown: {text:?}");
        assert!(text.contains("src/main.rs"), "candidate listed: {text:?}");
        assert!(
            text.contains("> src/main.rs"),
            "top candidate is selected: {text:?}"
        );
    }

    #[test]
    fn mention_dropdown_marks_dirs() {
        let mut app = App::new();
        app.input.insert('@');
        app.open_mention();
        app.files_loaded(vec!["src/".into(), "a.rs".into()], false);
        let text = render(&app);
        // The trailing slash is the visible directory marker.
        assert!(
            text.contains("src/"),
            "dir shown with trailing slash: {text:?}"
        );
    }

    #[test]
    fn text_input_overlay_lists_plan_candidates_when_mention_active() {
        let mut app = App::new();
        app.open_text_input("SDD plan file", "sdd");
        app.text_input_char('@');
        app.files_loaded(vec![".otto/plans/p.md".into(), "src/x.rs".into()], false);
        let text = render(&app);
        assert!(
            text.contains(".otto/plans/p.md"),
            "plan candidate listed: {text:?}"
        );
        assert!(
            text.contains("tab/enter insert"),
            "mention hint shown: {text:?}"
        );
    }

    #[test]
    fn input_stays_focused_with_mention_open() {
        use ratatui::{Terminal, backend::TestBackend};
        // Baseline: identical buffer, no overlay → the focused cursor position.
        let mut base = App::new();
        base.input.insert('@');
        let mut term = Terminal::new(TestBackend::new(40, 3)).unwrap();
        term.draw(|f| input(&base, f, f.area())).unwrap();
        let cur_none = term.get_cursor_position().unwrap();

        // With the mention dropdown open the editor must keep the cursor.
        let mut men = App::new();
        men.input.insert('@');
        men.open_mention();
        let mut term2 = Terminal::new(TestBackend::new(40, 3)).unwrap();
        term2.draw(|f| input(&men, f, f.area())).unwrap();
        let cur_men = term2.get_cursor_position().unwrap();
        assert_eq!(
            cur_none, cur_men,
            "mention overlay keeps the editor focused (cursor unchanged)"
        );
    }
}
