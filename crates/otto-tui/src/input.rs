//! The hand-rolled multiline prompt editor and key routing.

use crate::state::{
    App, DashboardPeek, DashboardStatus, LoopAction, Msg, Overlay, QuestionReplyKind,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// One visual row produced by soft-wrapping a logical line: the logical line
/// it came from and the byte range `[start, end)` within that line. An empty
/// logical line yields a single `WrapRow` with `start == end == 0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WrapRow {
    pub logical_row: usize,
    pub start: usize,
    pub end: usize,
}

/// Break each logical line into visual rows that each fit within `width`
/// display columns (via `unicode-width`). Wide graphemes are never split: a
/// grapheme that would overflow starts the next row instead. `width == 0` is
/// treated as `1`. Pure — `Editor` stays width-agnostic.
#[must_use]
pub fn wrap_rows(lines: &[String], width: u16) -> Vec<WrapRow> {
    use unicode_width::UnicodeWidthChar;
    let width = width.max(1) as usize;
    let mut out = Vec::new();
    for (lr, line) in lines.iter().enumerate() {
        if line.is_empty() {
            out.push(WrapRow {
                logical_row: lr,
                start: 0,
                end: 0,
            });
            continue;
        }
        let mut start = 0usize;
        let mut cur_w = 0usize;
        for (idx, ch) in line.char_indices() {
            let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
            if cur_w + cw > width && idx > start {
                out.push(WrapRow {
                    logical_row: lr,
                    start,
                    end: idx,
                });
                start = idx;
                cur_w = 0;
            }
            cur_w += cw;
        }
        out.push(WrapRow {
            logical_row: lr,
            start,
            end: line.len(),
        });
    }
    out
}

/// A minimal multiline text buffer with a cursor. No wrapping, no undo.
///
/// `col` is a BYTE offset into `lines[row]`, not a character count. This
/// keeps it a valid index for `String::insert`/`remove`/`split_off`, which
/// all require UTF-8 char-boundary indices.
#[derive(Debug)]
pub struct Editor {
    lines: Vec<String>,
    row: usize,
    col: usize,
}

impl Editor {
    #[must_use]
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            row: 0,
            col: 0,
        }
    }

    pub fn insert(&mut self, c: char) {
        self.lines[self.row].insert(self.col, c);
        self.col += c.len_utf8();
    }

    pub fn newline(&mut self) {
        let rest = self.lines[self.row].split_off(self.col);
        self.lines.insert(self.row + 1, rest);
        self.row += 1;
        self.col = 0;
    }

    pub fn backspace(&mut self) {
        if self.col > 0 {
            let prev = self.lines[self.row][..self.col]
                .chars()
                .next_back()
                .expect("col > 0 implies a preceding char");
            self.col -= prev.len_utf8();
            self.lines[self.row].remove(self.col);
        } else if self.row > 0 {
            let cur = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.lines[self.row].len();
            self.lines[self.row].push_str(&cur);
        }
    }

    /// The character immediately before the cursor on the current row, if any.
    /// Used to gate the `@`-mention trigger to word boundaries (so `foo@bar`
    /// emails type literally).
    #[must_use]
    pub fn prev_char(&self) -> Option<char> {
        self.lines[self.row][..self.col].chars().next_back()
    }

    /// Replace `lines[row][start_col..cursor]` with `text`, then place the
    /// cursor just past the inserted text. Used to swap an `@partial` token for
    /// the accepted `@path`. `start_col` and the cursor must be char
    /// boundaries on `row` (the mention layer only ever calls this on the
    /// cursor's own row, so `col` is valid).
    pub fn replace_to_cursor(&mut self, row: usize, start_col: usize, text: &str) {
        self.lines[row].replace_range(start_col..self.col, text);
        self.row = row;
        self.col = start_col + text.len();
    }

    #[must_use]
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn take(&mut self) -> String {
        let t = self.text();
        *self = Self::new();
        t
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    #[must_use]
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// Returns `(row, col)`. `col` is a byte offset, not a character count.
    #[must_use]
    pub fn cursor(&self) -> (usize, usize) {
        (self.row, self.col)
    }

    /// Returns `(row, display_col)` — the cursor's row and its column in
    /// terminal display cells (byte offset widened via `unicode-width`, so
    /// CJK/emoji before the cursor advance it correctly).
    #[must_use]
    pub fn cursor_display(&self) -> (usize, u16) {
        use unicode_width::UnicodeWidthStr;
        let col = UnicodeWidthStr::width(&self.lines[self.row][..self.col]) as u16;
        (self.row, col)
    }

    /// The cursor's `(visual_row, display_col)` under soft-wrapping at
    /// `width` display columns — the same wrapping `wrap_rows` produces, so
    /// rendering and cursor placement never disagree. A cursor at the end of
    /// a non-final fragment maps to column 0 of the following visual row.
    #[must_use]
    pub fn cursor_visual(&self, width: u16) -> (u16, u16) {
        use unicode_width::UnicodeWidthStr;
        let rows = wrap_rows(&self.lines, width);
        for (vr, w) in rows.iter().enumerate() {
            if w.logical_row != self.row {
                continue;
            }
            let is_last_frag = rows.get(vr + 1).is_none_or(|n| n.logical_row != self.row);
            let in_range =
                self.col >= w.start && (self.col < w.end || (self.col == w.end && is_last_frag));
            if in_range {
                let col = UnicodeWidthStr::width(&self.lines[self.row][w.start..self.col]) as u16;
                return (vr as u16, col);
            }
        }
        ((rows.len().saturating_sub(1)) as u16, 0)
    }
}

impl Default for Editor {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    /// Route a key event given the current overlay/focus. Returns a `Msg` for
    /// the loop to act on, or `None` when handled internally.
    pub fn on_key(&mut self, key: KeyEvent) -> Option<Msg> {
        // Startup splash: any key dismisses it and is swallowed (not treated as
        // input), so the first keystroke just skips the splash.
        if self.splash.is_some() {
            self.splash = None;
            return None;
        }
        // Global quit.
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Some(Msg::Quit);
        }
        // Global: start a fresh session (clears the transcript / context).
        if key.code == KeyCode::Char('n') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Some(Msg::NewSession);
        }
        // Global: suspend to the shell (SIGTSTP). Raw mode delivers ctrl+z as a
        // key (ISIG is off), so we self-suspend from the event loop.
        #[cfg(unix)]
        if key.code == KeyCode::Char('z') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.pending_action = Some(LoopAction::Suspend);
            return None;
        }
        // Global: open the command palette (only when no overlay is active,
        // so it never hijacks an already-open picker/permission prompt).
        if key.code == KeyCode::Char('k') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if matches!(self.overlay, Overlay::None) {
                self.open_palette();
            }
            return None;
        }
        // Global: open the file-attachment picker (only when no overlay is
        // active, mirroring ctrl+k).
        if key.code == KeyCode::Char('f') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if matches!(self.overlay, Overlay::None) {
                self.open_file_picker();
            }
            return None;
        }
        // Global: cancel the in-flight workflow. Fires only while a run is
        // active (`workflow` present and not yet `done`); the loop performs the
        // HTTP cancel (dispatch) — this key just emits the intent. Swallowed
        // (no fall-through to editor) so it never types a stray 'x'.
        if key.code == KeyCode::Char('x') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if let Some(w) = self.workflow.as_ref().filter(|w| w.done.is_none()) {
                return Some(Msg::CancelWorkflow(w.session.clone()));
            }
            return None;
        }
        // Global: toggle the workflow status panel. Opens when a run exists,
        // closes when already open (see `toggle_workflow_status`). Works whether
        // or not the panel is the active overlay, so it lands before the
        // overlay-scoped block.
        if key.code == KeyCode::Char('w') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.toggle_workflow_status();
            return None;
        }
        // Overlay-scoped keys.
        if !matches!(self.overlay, Overlay::None) {
            if matches!(self.overlay, Overlay::Files(_)) {
                match key.code {
                    KeyCode::Esc => self.close_overlay(),
                    KeyCode::Enter => self.file_toggle(),
                    KeyCode::Up => self.file_move(-1),
                    KeyCode::Down => self.file_move(1),
                    KeyCode::Backspace => self.file_backspace(),
                    KeyCode::Char(c)
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                    {
                        self.file_input(c)
                    }
                    _ => {}
                }
                return None;
            }
            if matches!(self.overlay, Overlay::Palette(_)) {
                match key.code {
                    KeyCode::Esc => self.close_overlay(),
                    KeyCode::Enter => return self.palette_confirm(),
                    KeyCode::Up => self.palette_move(-1),
                    KeyCode::Down => self.palette_move(1),
                    KeyCode::Backspace => self.palette_backspace(),
                    // Only unmodified chars (Shift-only, for capitals, is
                    // fine — it's not in this mask) insert into the query;
                    // ctrl/alt chords (e.g. a stray ctrl+k while typing)
                    // must not leak a bare letter into the palette input.
                    KeyCode::Char(c)
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                    {
                        self.palette_input(c)
                    }
                    _ => {}
                }
                return None;
            }
            if matches!(self.overlay, Overlay::TextInput(_)) {
                let mention_active =
                    matches!(&self.overlay, Overlay::TextInput(s) if s.mention.is_some());
                match key.code {
                    // While a mention is active, ↑↓ move the highlight and
                    // tab/enter accept it — Enter must NOT fall through to
                    // `text_input_confirm` and start the workflow. Esc dismisses
                    // just the mention (a second Esc then closes the overlay).
                    KeyCode::Up if mention_active => self.text_input_mention_move(-1),
                    KeyCode::Down if mention_active => self.text_input_mention_move(1),
                    KeyCode::Tab | KeyCode::Enter if mention_active => {
                        self.text_input_mention_accept()
                    }
                    KeyCode::Esc if mention_active => self.text_input_clear_mention(),
                    KeyCode::Esc => self.close_overlay(),
                    KeyCode::Enter => return self.text_input_confirm(),
                    KeyCode::Backspace => self.text_input_backspace(),
                    // Only unmodified chars insert into the query; ctrl/alt
                    // chords must not leak a bare letter (mirrors the palette).
                    KeyCode::Char(c)
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                    {
                        self.text_input_char(c)
                    }
                    _ => {}
                }
                return None;
            }
            if matches!(self.overlay, Overlay::Search(_)) {
                match key.code {
                    KeyCode::Esc => self.close_overlay(),
                    // Enter is a deliberate no-op: search stays open (there's
                    // nothing else for it to "confirm" — see task brief).
                    KeyCode::Enter => {}
                    KeyCode::Backspace => self.search_backspace(),
                    // `n`/`N` are intercepted for next/previous-match
                    // navigation, so a literal `n`/`N` can never be typed
                    // into the pattern. Patterns rarely need a literal n —
                    // an accepted tradeoff for a single-key jump binding.
                    KeyCode::Char('n')
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                    {
                        self.search_move(1)
                    }
                    KeyCode::Char('N')
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                    {
                        self.search_move(-1)
                    }
                    KeyCode::Char(c)
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                    {
                        self.search_input(c)
                    }
                    _ => {}
                }
                return None;
            }
            if matches!(self.overlay, Overlay::Mention(_)) {
                let shift = key.modifiers.contains(KeyModifiers::SHIFT);
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                match key.code {
                    KeyCode::Esc => self.close_overlay(),
                    KeyCode::Up => self.mention_move(-1),
                    KeyCode::Down => self.mention_move(1),
                    // shift+enter / ctrl+j: newline in the buffer, dismiss the
                    // dropdown (a deliberate line break ends the token).
                    KeyCode::Enter if shift => {
                        self.input.newline();
                        self.close_overlay();
                    }
                    KeyCode::Char('j') if ctrl => {
                        self.input.newline();
                        self.close_overlay();
                    }
                    // Enter/Tab accept the highlight; on no match this only
                    // dismisses — it NEVER submits the message.
                    KeyCode::Enter | KeyCode::Tab => self.mention_accept(),
                    KeyCode::Backspace => {
                        self.input.backspace();
                        self.mention_after_edit();
                    }
                    // A space delimits the token: type it and dismiss.
                    KeyCode::Char(' ') => {
                        self.input.insert(' ');
                        self.close_overlay();
                    }
                    KeyCode::Char(c)
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                    {
                        self.input.insert(c);
                        self.mention_after_edit();
                    }
                    // Any other key dismisses and is swallowed.
                    _ => self.close_overlay(),
                }
                return None;
            }
            match key.code {
                KeyCode::Esc if self.is_question() => return self.question_cancel(),
                KeyCode::Esc => self.close_overlay(),
                KeyCode::Char('y') if self.is_permission() => return self.reply_intent("once"),
                KeyCode::Char('a') if self.is_permission() => return self.reply_intent("always"),
                KeyCode::Char('n') if self.is_permission() => return self.reply_intent("reject"),
                KeyCode::Char('y') if self.dashboard_awaiting_permission() => {
                    return self.dashboard_reply_permission("once");
                }
                KeyCode::Char('a') if self.dashboard_awaiting_permission() => {
                    return self.dashboard_reply_permission("always");
                }
                KeyCode::Char('n') if self.dashboard_awaiting_permission() => {
                    return self.dashboard_reply_permission("reject");
                }
                KeyCode::Char(c)
                    if c.is_ascii_digit() && c != '0' && self.dashboard_awaiting_question() =>
                {
                    return self.dashboard_reply_question(c.to_digit(10).unwrap() as usize - 1);
                }
                KeyCode::Up if self.is_dashboard() => {
                    self.dashboard_move(-1);
                    return None;
                }
                KeyCode::Down if self.is_dashboard() => {
                    self.dashboard_move(1);
                    return None;
                }
                KeyCode::Enter if self.is_dashboard() => return self.dashboard_open_selected(),
                KeyCode::Up if self.is_question() => {
                    self.question_move_highlight(-1);
                }
                KeyCode::Down if self.is_question() => {
                    self.question_move_highlight(1);
                }
                KeyCode::Char(' ') if self.is_question() => {
                    self.question_toggle_highlighted();
                }
                KeyCode::Enter if self.is_question() => return self.question_confirm(),
                KeyCode::Up if self.picker_len() > 0 => {
                    self.picker_move(-1);
                }
                KeyCode::Down if self.picker_len() > 0 => {
                    self.picker_move(1);
                }
                KeyCode::Enter if self.picker_len() > 0 => return self.picker_confirm(),
                _ => {}
            }
            return None;
        }
        // Editor / main focus.
        match key.code {
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.input.newline();
                None
            }
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.newline();
                None
            }
            KeyCode::Enter => {
                // Empty input + a selected tool → toggle it (empty Enter was a
                // no-op before). Otherwise submit as usual.
                if self.input.is_empty() {
                    if self.tool_cursor.is_some() {
                        self.toggle_selected_or_last_tool();
                    }
                    return None;
                }
                // One prompt stream at a time: submitting mid-turn would spawn
                // a second concurrent SSE stream whose deltas interleave into
                // the same transcript (garbled/duplicated blocks). Keep the
                // typed text in the editor and tell the user why.
                if self.turn_in_flight() {
                    self.flash_warning("turn in flight — Esc to interrupt it first");
                    return None;
                }
                Some(Msg::Submitted(self.input.take()))
            }
            // Esc while a turn is streaming interrupts it (keeps the session).
            // Checked before the tool-cursor/overlay Esc handling so it wins
            // whenever the agent is busy.
            KeyCode::Esc if self.turn_in_flight() => {
                self.session_id.clone().map(Msg::InterruptTurn)
            }
            KeyCode::Esc if self.input.is_empty() && self.tool_cursor.is_some() => {
                self.tool_cursor = None;
                self.scroll_to_bottom();
                None
            }
            KeyCode::Up if self.input.is_empty() => {
                self.select_prev_tool();
                None
            }
            KeyCode::Down if self.input.is_empty() => {
                self.select_next_tool();
                None
            }
            KeyCode::Backspace => {
                self.input.backspace();
                None
            }
            // `?` (help) and `/` (search) stay bare-when-empty: non-letter keys
            // that don't shadow ordinary typing. The former letter shortcuts
            // (t/m/g/s/o/q/y) moved to ctrl chords below so a message can start
            // with any letter; models/sessions also live in the ctrl+k palette.
            KeyCode::Char('?') if self.input.is_empty() => {
                self.overlay = Overlay::Help;
                None
            }
            KeyCode::Char('/') if self.input.is_empty() => {
                self.open_search();
                None
            }
            KeyCode::BackTab => Some(Msg::CyclePermissionMode),
            // ctrl chords for quick actions (collision-free: ctrl+m == Enter and
            // ctrl+s == flow-control, so models/sessions stay on the palette).
            KeyCode::Char('t') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(Msg::ToggleTool)
            }
            KeyCode::Char('g') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.open_picker(Overlay::Agents);
                None
            }
            KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(Msg::ToggleTodos)
            }
            KeyCode::Char('y') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.pending_action = Some(LoopAction::Yank);
                None
            }
            KeyCode::PageUp => Some(Msg::ScrollUp),
            KeyCode::PageDown => Some(Msg::ScrollDown),
            KeyCode::End if self.input.is_empty() => Some(Msg::ScrollBottom),
            // `@` at a word boundary opens inline file/folder completion; a
            // mid-word `@` (e.g. an email) types literally.
            KeyCode::Char('@')
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    && self.input.prev_char().is_none_or(char::is_whitespace) =>
            {
                self.input.insert('@');
                self.open_mention();
                None
            }
            KeyCode::Char(c) => {
                self.input.insert(c);
                None
            }
            _ => None,
        }
    }

    fn is_permission(&self) -> bool {
        matches!(self.overlay, Overlay::Permission(_))
    }

    fn is_dashboard(&self) -> bool {
        matches!(self.overlay, Overlay::Dashboard)
    }

    fn dashboard_awaiting_permission(&self) -> bool {
        self.is_dashboard() && matches!(self.dashboard.peek, DashboardPeek::Permission)
    }

    fn dashboard_awaiting_question(&self) -> bool {
        self.is_dashboard() && matches!(self.dashboard.peek, DashboardPeek::Question { .. })
    }

    /// Move the dashboard's row selection by `delta` (clamped), resetting
    /// the peek to whatever `derive_peek` says for the new row —
    /// `route_message` (Task 8) observes a `Loading` result after this and
    /// spawns the actual message fetch for idle/busy rows.
    fn dashboard_move(&mut self, delta: isize) {
        let len = self.dashboard.rows.len();
        if len == 0 {
            return;
        }
        let max = len - 1;
        let next = (self.dashboard.selected as isize + delta).clamp(0, max as isize);
        self.dashboard.selected = next as usize;
        self.dashboard.peek = self.dashboard.derive_peek();
    }

    /// Open (fully switch to) the currently-selected dashboard row's
    /// session, closing the dashboard.
    fn dashboard_open_selected(&mut self) -> Option<Msg> {
        let row = self.dashboard.rows.get(self.dashboard.selected)?;
        let id = row.session.id.clone();
        self.close_overlay();
        Some(Msg::SwitchSession(id))
    }

    /// Reply `reply` (`"once"`/`"always"`/`"reject"`) to the selected row's
    /// pending permission ask.
    fn dashboard_reply_permission(&mut self, reply: &str) -> Option<Msg> {
        let DashboardStatus::AwaitingPermission(p) =
            &self.dashboard.rows.get(self.dashboard.selected)?.status
        else {
            return None;
        };
        Some(Msg::PermissionReply {
            id: p.id.clone(),
            reply: reply.to_string(),
        })
    }

    /// Answer the selected row's pending single-question ask with option
    /// `index`. `dashboard_awaiting_question` already guarantees exactly
    /// one non-multiple question, so a single-element `Answered` payload is
    /// always the right shape; an out-of-range `index` is ignored.
    fn dashboard_reply_question(&mut self, index: usize) -> Option<Msg> {
        let DashboardStatus::AwaitingQuestion(q) =
            &self.dashboard.rows.get(self.dashboard.selected)?.status
        else {
            return None;
        };
        let question = q.questions.first()?;
        if index >= question.options.len() {
            return None;
        }
        Some(Msg::QuestionReply {
            id: q.id.clone(),
            reply: QuestionReplyKind::Answered(vec![vec![index]]),
        })
    }

    /// Produce a permission-reply intent `Msg` and close the overlay.
    fn reply_intent(&mut self, reply: &str) -> Option<Msg> {
        if let Overlay::Permission(p) = &self.overlay {
            let id = p.id.clone();
            self.close_overlay();
            return Some(Msg::PermissionReply {
                id,
                reply: reply.to_string(),
            });
        }
        None
    }

    fn is_question(&self) -> bool {
        matches!(self.overlay, Overlay::Question(_))
    }

    fn question_move_highlight(&mut self, delta: i32) {
        if let Overlay::Question(qs) = &mut self.overlay {
            qs.move_highlight(delta);
        }
    }

    fn question_toggle_highlighted(&mut self) {
        if let Overlay::Question(qs) = &mut self.overlay {
            let idx = qs.highlight;
            qs.toggle(idx);
        }
    }

    /// Confirm the highlighted/toggled selection for the current question.
    /// For a single-select question, Enter both selects the highlighted
    /// option and confirms in one step (mirrors `reply_intent`'s
    /// one-keypress-per-decision feel) — `toggle` is called first if the
    /// cursor is still empty, so a bare Enter on a fresh single-select
    /// question answers with whatever's highlighted rather than doing
    /// nothing.
    fn question_confirm(&mut self) -> Option<Msg> {
        if let Overlay::Question(qs) = &mut self.overlay {
            if qs.cursor.is_empty() && !qs.current_question_is_multiple() {
                let idx = qs.highlight;
                qs.toggle(idx);
            }
            let done = qs.confirm_current();
            if done {
                let id = qs.id.clone();
                let answers = qs.answers.clone();
                self.close_overlay();
                return Some(Msg::QuestionReply {
                    id,
                    reply: QuestionReplyKind::Answered(answers),
                });
            }
        }
        None
    }

    fn question_cancel(&mut self) -> Option<Msg> {
        if let Overlay::Question(qs) = &self.overlay {
            let id = qs.id.clone();
            self.close_overlay();
            return Some(Msg::QuestionReply {
                id,
                reply: QuestionReplyKind::Cancelled,
            });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::DashboardRow;

    #[test]
    fn insert_backspace_newline_and_take() {
        let mut e = Editor::new();
        for c in "hi".chars() {
            e.insert(c);
        }
        e.newline();
        for c in "yo".chars() {
            e.insert(c);
        }
        assert_eq!(e.text(), "hi\nyo");
        e.backspace();
        assert_eq!(e.text(), "hi\ny");
        let taken = e.take();
        assert_eq!(taken, "hi\ny");
        assert!(e.is_empty());
    }

    #[test]
    fn backspace_at_line_start_joins_lines() {
        let mut e = Editor::new();
        for c in "ab".chars() {
            e.insert(c);
        }
        e.newline();
        // cursor at start of line 2; backspace joins.
        e.backspace();
        assert_eq!(e.text(), "ab");
    }

    #[cfg(unix)]
    #[test]
    fn ctrl_z_sets_pending_suspend() {
        let mut app = App::new();
        let out = app.on_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL));
        assert!(out.is_none());
        assert_eq!(app.pending_action, Some(LoopAction::Suspend));
    }

    #[test]
    fn ctrl_y_sets_pending_yank() {
        let mut app = App::new();
        let out = app.on_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL));
        assert!(out.is_none());
        assert_eq!(app.pending_action, Some(LoopAction::Yank));
    }

    #[test]
    fn esc_interrupts_turn_when_generating() {
        let mut app = App::new();
        app.session_id = Some("ses_1".into());
        app.status = "…thinking".into();
        let out = app.on_key(key(KeyCode::Esc));
        assert!(matches!(out, Some(crate::state::Msg::InterruptTurn(s)) if s == "ses_1"));
    }

    #[test]
    fn y_inserts_char_when_input_nonempty() {
        let mut app = App::new();
        app.input.insert('a');
        let msg = app.on_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(msg.is_none());
        assert_eq!(app.input.text(), "ay");
    }

    #[test]
    fn y_types_when_input_nonempty() {
        let mut app = App::new();
        app.input.insert('x');
        let _ = app.on_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert_eq!(app.pending_action, None);
        assert_eq!(app.input.text(), "xy");
    }

    #[test]
    fn backtab_cycles_permission_mode_when_no_overlay() {
        let mut app = App::new();
        let msg = app.on_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE));
        assert!(matches!(msg, Some(Msg::CyclePermissionMode)));
    }

    #[test]
    fn slash_opens_search_when_input_empty() {
        let mut app = App::new();
        let msg = app.on_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        assert!(msg.is_none());
        assert!(matches!(app.overlay, Overlay::Search(_)));
    }

    #[test]
    fn slash_inserts_char_when_input_nonempty() {
        let mut app = App::new();
        app.input.insert('a');
        let msg = app.on_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        assert!(msg.is_none());
        assert_eq!(app.input.text(), "a/");
        assert!(matches!(app.overlay, Overlay::None));
    }

    #[test]
    fn search_overlay_esc_closes_and_enter_is_noop() {
        let mut app = App::new();
        app.open_search();
        let msg = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(msg.is_none());
        assert!(
            matches!(app.overlay, Overlay::Search(_)),
            "enter must not close search"
        );
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(app.overlay, Overlay::None));
    }

    #[test]
    fn search_overlay_types_query_and_backspaces() {
        let mut app = App::new();
        app.open_search();
        for c in "err".chars() {
            app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        match &app.overlay {
            crate::state::Overlay::Search(s) => assert_eq!(s.query, "err"),
            other => panic!("expected search overlay, got {other:?}"),
        }
        app.on_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        match &app.overlay {
            crate::state::Overlay::Search(s) => assert_eq!(s.query, "er"),
            other => panic!("expected search overlay, got {other:?}"),
        }
    }

    #[test]
    fn search_overlay_n_and_shift_n_navigate_matches() {
        let mut app = App::new();
        app.open_search();
        let msg = app.on_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(msg.is_none());
        match &app.overlay {
            crate::state::Overlay::Search(s) => assert_eq!(s.current, 1),
            other => panic!("expected search overlay, got {other:?}"),
        }
        // Shift-N arrives as `Char('N')`.
        let msg = app.on_key(KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT));
        assert!(msg.is_none());
        match &app.overlay {
            crate::state::Overlay::Search(s) => assert_eq!(s.current, 0),
            other => panic!("expected search overlay, got {other:?}"),
        }
        // A literal n/N never reaches the query (documented tradeoff).
        match &app.overlay {
            crate::state::Overlay::Search(s) => assert!(s.query.is_empty()),
            other => panic!("expected search overlay, got {other:?}"),
        }
    }

    #[test]
    fn handles_multibyte_chars_without_panic() {
        let mut e = Editor::new();
        for c in "héllo".chars() {
            e.insert(c);
        }
        e.insert('x');
        assert_eq!(e.text(), "héllox");
        e.backspace(); // removes 'x'
        e.backspace(); // removes 'o'
        assert_eq!(e.text(), "héll");
        e.newline();
        e.insert('界');
        assert_eq!(e.text(), "héll\n界");
    }

    #[test]
    fn cursor_display_counts_display_width() {
        let mut e = Editor::new();
        assert_eq!(e.cursor_display(), (0, 0));
        e.insert('a');
        e.insert('b');
        assert_eq!(e.cursor_display(), (0, 2));
        // A CJK ideograph is display-width 2 though it is one char / 3 bytes.
        e.insert('世');
        assert_eq!(e.cursor_display(), (0, 4));
        e.newline();
        assert_eq!(e.cursor_display(), (1, 0));
    }

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn app_with_two_tools() -> App {
        let mut app = App::new();
        for t in ["a", "b"] {
            app.transcript.push(crate::state::TranscriptItem::Tool {
                name: "read".into(),
                status: crate::state::ToolStatus::Ok,
                title: t.into(),
                input: None,
                output: None,
                expanded: false,
            });
        }
        app
    }

    #[test]
    fn splash_swallows_first_key_and_dismisses() {
        let mut app = App::new();
        app.splash = Some(crate::splash::SPLASH_TICKS);
        // Even a normally-actionable key (Enter) is swallowed while the splash
        // is up: it only dismisses the splash, producing no Msg.
        assert!(app.on_key(key(KeyCode::Enter)).is_none());
        assert!(app.splash.is_none(), "splash dismissed");
        // Once dismissed, the same key behaves normally again (Enter on an
        // empty prompt is a no-op → None, but the splash no longer intercepts).
        let _ = app.on_key(key(KeyCode::Enter));
        assert!(app.splash.is_none());
    }

    #[test]
    fn down_up_navigate_tools_when_empty() {
        let mut app = app_with_two_tools();
        assert!(app.on_key(key(KeyCode::Up)).is_none());
        assert_eq!(app.tool_cursor, Some(1)); // newest
        let _ = app.on_key(key(KeyCode::Up));
        assert_eq!(app.tool_cursor, Some(0));
    }

    #[test]
    fn enter_toggles_selected_tool_but_still_submits_with_text() {
        let mut app = app_with_two_tools();
        app.tool_cursor = Some(0);
        assert!(app.on_key(key(KeyCode::Enter)).is_none()); // toggle, not submit
        assert!(matches!(
            app.transcript[0],
            crate::state::TranscriptItem::Tool { expanded: true, .. }
        ));
        // With text in the buffer, Enter still submits.
        app.input.insert('x');
        match app.on_key(key(KeyCode::Enter)) {
            Some(crate::state::Msg::Submitted(t)) => assert_eq!(t, "x"),
            other => panic!("expected Submitted, got {other:?}"),
        }
    }

    #[test]
    fn esc_clears_selection_when_empty() {
        let mut app = app_with_two_tools();
        app.tool_cursor = Some(1);
        assert!(app.on_key(key(KeyCode::Esc)).is_none());
        assert_eq!(app.tool_cursor, None);
    }

    #[test]
    fn wrap_short_line_is_one_row() {
        let lines = vec!["hello".to_string()];
        let rows = wrap_rows(&lines, 10);
        assert_eq!(
            rows,
            vec![WrapRow {
                logical_row: 0,
                start: 0,
                end: 5
            }]
        );
    }

    #[test]
    fn wrap_line_exactly_width_is_one_row() {
        let lines = vec!["abcde".to_string()];
        let rows = wrap_rows(&lines, 5);
        assert_eq!(
            rows,
            vec![WrapRow {
                logical_row: 0,
                start: 0,
                end: 5
            }]
        );
    }

    #[test]
    fn wrap_line_over_width_splits() {
        let lines = vec!["abcdef".to_string()];
        let rows = wrap_rows(&lines, 5);
        assert_eq!(
            rows,
            vec![
                WrapRow {
                    logical_row: 0,
                    start: 0,
                    end: 5
                },
                WrapRow {
                    logical_row: 0,
                    start: 5,
                    end: 6
                },
            ]
        );
    }

    #[test]
    fn wrap_empty_line_yields_one_empty_row() {
        let lines = vec![String::new()];
        let rows = wrap_rows(&lines, 5);
        assert_eq!(
            rows,
            vec![WrapRow {
                logical_row: 0,
                start: 0,
                end: 0
            }]
        );
    }

    #[test]
    fn wrap_multiple_logical_lines() {
        let lines = vec!["ab".to_string(), "cd".to_string()];
        let rows = wrap_rows(&lines, 5);
        assert_eq!(
            rows,
            vec![
                WrapRow {
                    logical_row: 0,
                    start: 0,
                    end: 2
                },
                WrapRow {
                    logical_row: 1,
                    start: 0,
                    end: 2
                },
            ]
        );
    }

    #[test]
    fn wrap_wide_grapheme_not_split_at_boundary() {
        // Each CJK char is display width 2. Width 3 fits one CJK (2) then must
        // break before the next (would be 4 > 3). Bytes: each char is 3 bytes.
        let lines = vec!["世界".to_string()];
        let rows = wrap_rows(&lines, 3);
        assert_eq!(
            rows,
            vec![
                WrapRow {
                    logical_row: 0,
                    start: 0,
                    end: 3
                },
                WrapRow {
                    logical_row: 0,
                    start: 3,
                    end: 6
                },
            ]
        );
    }

    #[test]
    fn wrap_zero_width_treated_as_one() {
        let lines = vec!["ab".to_string()];
        let rows = wrap_rows(&lines, 0);
        assert_eq!(
            rows,
            vec![
                WrapRow {
                    logical_row: 0,
                    start: 0,
                    end: 1
                },
                WrapRow {
                    logical_row: 0,
                    start: 1,
                    end: 2
                },
            ]
        );
    }

    #[test]
    fn cursor_visual_start_of_empty() {
        let e = Editor::new();
        assert_eq!(e.cursor_visual(10), (0, 0));
    }

    #[test]
    fn cursor_visual_after_typing() {
        let mut e = Editor::new();
        for c in "abc".chars() {
            e.insert(c);
        }
        assert_eq!(e.cursor_visual(10), (0, 3));
    }

    #[test]
    fn cursor_visual_after_newline() {
        let mut e = Editor::new();
        e.insert('a');
        e.newline();
        e.insert('b');
        // Second logical line, one char in.
        assert_eq!(e.cursor_visual(10), (1, 1));
    }

    #[test]
    fn cursor_visual_wraps_to_next_row() {
        let mut e = Editor::new();
        for c in "abcdef".chars() {
            e.insert(c);
        }
        // width 5: "abcde" then "f"; cursor after 'f' is row 1, col 1.
        assert_eq!(e.cursor_visual(5), (1, 1));
    }

    #[test]
    fn cursor_visual_at_wrap_boundary_is_next_row_col0() {
        let mut e = Editor::new();
        for c in "abcdef".chars() {
            e.insert(c);
        }
        // Place the cursor at byte 5 — the END of the first fragment "abcde"
        // ([0,5) at width 5), which is NON-final because "f" follows. This is
        // only reachable by setting the field directly (no cursor-left method);
        // legal here because `mod tests` is a child module of Editor's module.
        e.col = 5;
        // Boundary rule: end of a non-final fragment maps to col 0 of the next
        // visual row.
        assert_eq!(e.cursor_visual(5), (1, 0));
    }

    #[test]
    fn cursor_visual_cjk_display_col() {
        let mut e = Editor::new();
        e.insert('世'); // width 2
        // width 10: one row, cursor at display col 2.
        assert_eq!(e.cursor_visual(10), (0, 2));
    }

    #[test]
    fn ctrl_j_inserts_newline() {
        let mut app = App::new();
        app.input.insert('a');
        let out = app.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        assert!(out.is_none()); // handled internally, not submitted
        assert_eq!(app.input.text(), "a\n");
    }

    // ----- Task D: inline `@` file/folder mention -------------------------

    #[test]
    fn at_opens_mention_when_empty() {
        let mut app = App::new();
        assert!(app.on_key(key(KeyCode::Char('@'))).is_none());
        assert!(matches!(app.overlay, Overlay::Mention(_)));
        assert_eq!(app.input.text(), "@");
    }

    #[test]
    fn at_after_space_opens_mention() {
        let mut app = App::new();
        for c in "hi ".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Char('@')));
        assert!(matches!(app.overlay, Overlay::Mention(_)));
        assert_eq!(app.input.text(), "hi @");
    }

    #[test]
    fn at_mid_word_is_literal() {
        let mut app = App::new();
        for c in "foo".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Char('@'))); // email-style, no boundary
        assert!(
            matches!(app.overlay, Overlay::None),
            "mid-word @ is literal"
        );
        assert_eq!(app.input.text(), "foo@");
    }

    #[test]
    fn mention_enter_accepts_not_submits() {
        let mut app = App::new();
        app.session_id = Some("ses_1".into()); // a submit would be possible
        app.on_key(key(KeyCode::Char('@')));
        app.files_loaded(vec!["a.rs".into()], false);
        let msg = app.on_key(key(KeyCode::Enter));
        assert!(msg.is_none(), "enter inside a mention never submits");
        assert_eq!(app.input.text(), "@a.rs ");
        assert!(matches!(app.overlay, Overlay::None));
    }

    #[test]
    fn mention_tab_accepts() {
        let mut app = App::new();
        app.on_key(key(KeyCode::Char('@')));
        app.files_loaded(vec!["a.rs".into()], false);
        assert!(app.on_key(key(KeyCode::Tab)).is_none());
        assert_eq!(app.input.text(), "@a.rs ");
        assert!(matches!(app.overlay, Overlay::None));
    }

    #[test]
    fn mention_esc_keeps_typed_text() {
        let mut app = App::new();
        app.on_key(key(KeyCode::Char('@')));
        for c in "src".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Esc));
        assert!(matches!(app.overlay, Overlay::None));
        assert_eq!(app.input.text(), "@src", "typed text survives dismissal");
    }

    #[test]
    fn mention_space_dismisses() {
        let mut app = App::new();
        app.on_key(key(KeyCode::Char('@')));
        for c in "sr".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Char(' ')));
        assert!(
            matches!(app.overlay, Overlay::None),
            "space delimits the token"
        );
        assert_eq!(app.input.text(), "@sr ");
    }

    #[test]
    fn mention_shift_enter_inserts_newline_and_dismisses() {
        let mut app = App::new();
        app.on_key(key(KeyCode::Char('@')));
        for c in "sr".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        assert!(matches!(app.overlay, Overlay::None));
        assert_eq!(app.input.text(), "@sr\n");
    }

    #[test]
    fn ctrl_c_quits_while_mention_open() {
        let mut app = App::new();
        app.on_key(key(KeyCode::Char('@')));
        let msg = app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(
            matches!(msg, Some(Msg::Quit)),
            "ctrl+c wins over the overlay"
        );
    }

    // ----- Task 8: question overlay key handling ---------------------------

    fn sample_question_asked() -> crate::sse::QuestionAsked {
        crate::sse::QuestionAsked {
            id: "que_1".into(),
            session_id: "ses_1".into(),
            questions: vec![crate::sse::QuestionPromptView {
                question: "Pick one".into(),
                header: "choice".into(),
                options: vec![
                    crate::sse::QuestionOptionView {
                        label: "A".into(),
                        description: "a".into(),
                    },
                    crate::sse::QuestionOptionView {
                        label: "B".into(),
                        description: "b".into(),
                    },
                ],
                multiple: false,
            }],
        }
    }

    #[test]
    fn question_arrow_and_enter_answers_single_question() {
        use crate::state::{QuestionReplyKind, QuestionSession};
        let mut app = App::new();
        app.overlay = Overlay::Question(QuestionSession::new(sample_question_asked()));
        app.on_key(key(KeyCode::Down)); // highlight moves to option B
        let msg = app.on_key(key(KeyCode::Enter));
        match msg {
            Some(Msg::QuestionReply {
                id,
                reply: QuestionReplyKind::Answered(answers),
            }) => {
                assert_eq!(id, "que_1");
                assert_eq!(answers, vec![vec![1]]);
            }
            other => panic!("expected an Answered QuestionReply, got {other:?}"),
        }
        assert!(
            matches!(app.overlay, Overlay::None),
            "overlay closes on the final answer"
        );
    }

    #[test]
    fn question_esc_cancels() {
        use crate::state::{QuestionReplyKind, QuestionSession};
        let mut app = App::new();
        app.overlay = Overlay::Question(QuestionSession::new(sample_question_asked()));
        let msg = app.on_key(key(KeyCode::Esc));
        match msg {
            Some(Msg::QuestionReply {
                id,
                reply: QuestionReplyKind::Cancelled,
            }) => assert_eq!(id, "que_1"),
            other => panic!("expected a Cancelled QuestionReply, got {other:?}"),
        }
        assert!(matches!(app.overlay, Overlay::None));
    }

    // ----- Task 6: dashboard key handling -----------------------------------

    fn dash_perm(session_id: &str) -> crate::sse::PermissionAsked {
        crate::sse::PermissionAsked {
            id: format!("perm_{session_id}"),
            session_id: session_id.into(),
            permission: "edit".into(),
            patterns: vec!["*.rs".into()],
        }
    }

    fn dash_single_question(session_id: &str) -> crate::sse::QuestionAsked {
        crate::sse::QuestionAsked {
            id: format!("que_{session_id}"),
            session_id: session_id.into(),
            questions: vec![crate::sse::QuestionPromptView {
                question: "Pick one".into(),
                header: "choice".into(),
                options: vec![
                    crate::sse::QuestionOptionView {
                        label: "A".into(),
                        description: "a".into(),
                    },
                    crate::sse::QuestionOptionView {
                        label: "B".into(),
                        description: "b".into(),
                    },
                ],
                multiple: false,
            }],
        }
    }

    fn dash_session(id: &str) -> crate::client::SessionInfo {
        crate::client::SessionInfo {
            id: id.into(),
            ..Default::default()
        }
    }

    #[test]
    fn dashboard_arrows_move_selection_and_reset_peek() {
        let mut app = App::new();
        app.overlay = Overlay::Dashboard;
        app.dashboard.rows = vec![
            DashboardRow {
                session: dash_session("a"),
                status: DashboardStatus::Idle,
            },
            DashboardRow {
                session: dash_session("b"),
                status: DashboardStatus::Idle,
            },
        ];
        app.dashboard.selected = 0;
        let out = app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert!(out.is_none());
        assert_eq!(app.dashboard.selected, 1);
        assert_eq!(app.dashboard.peek, DashboardPeek::Loading);
    }

    #[test]
    fn dashboard_enter_opens_selected_session() {
        let mut app = App::new();
        app.overlay = Overlay::Dashboard;
        app.dashboard.rows = vec![DashboardRow {
            session: dash_session("target"),
            status: DashboardStatus::Idle,
        }];
        let out = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(out, Some(Msg::SwitchSession(id)) if id == "target"));
        assert!(
            matches!(app.overlay, Overlay::None),
            "dashboard closes when opening a session"
        );
    }

    #[test]
    fn dashboard_y_replies_once_to_pending_permission() {
        let mut app = App::new();
        app.overlay = Overlay::Dashboard;
        app.dashboard.rows = vec![DashboardRow {
            session: dash_session("s"),
            status: DashboardStatus::AwaitingPermission(dash_perm("s")),
        }];
        app.dashboard.peek = DashboardPeek::Permission;
        let out = app.on_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(matches!(
            out,
            Some(Msg::PermissionReply { id, reply })
                if id == "perm_s" && reply == "once"
        ));
    }

    #[test]
    fn dashboard_digit_replies_to_pending_question() {
        let mut app = App::new();
        app.overlay = Overlay::Dashboard;
        app.dashboard.rows = vec![DashboardRow {
            session: dash_session("s"),
            status: DashboardStatus::AwaitingQuestion(dash_single_question("s")),
        }];
        app.dashboard.peek = DashboardPeek::Question { highlight: 0 };
        let out = app.on_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
        assert!(matches!(
            out,
            Some(Msg::QuestionReply { id, reply: QuestionReplyKind::Answered(a) })
                if id == "que_s" && a == vec![vec![1]]
        ));
    }

    #[test]
    fn dashboard_digit_out_of_range_is_ignored() {
        let mut app = App::new();
        app.overlay = Overlay::Dashboard;
        app.dashboard.rows = vec![DashboardRow {
            session: dash_session("s"),
            status: DashboardStatus::AwaitingQuestion(dash_single_question("s")),
        }];
        app.dashboard.peek = DashboardPeek::Question { highlight: 0 };
        // dash_single_question has 2 options (indices 0-1); '9' -> index 8.
        let out = app.on_key(KeyEvent::new(KeyCode::Char('9'), KeyModifiers::NONE));
        assert!(out.is_none());
    }
}
