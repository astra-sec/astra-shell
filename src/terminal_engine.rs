use std::{collections::BTreeMap, io::Write, sync::Arc};

use anyhow::{Context, Result, ensure};
use astra_wezterm_term::color::{ColorAttribute, ColorPalette, SrgbaTuple};
use astra_wezterm_term::{
    AstraCellView, AstraCursorShape, AstraCursorView, AstraKeyboardEncoding, AstraModesView,
    AstraMouseEncoding, AstraMouseTracking, AstraRowView, AstraScreenKind, AstraScreenView,
    CellAttributes, Clipboard, Terminal, TerminalConfiguration, TerminalSize,
};
use uuid::Uuid;

use crate::protocol::TerminalSnapshot;
use crate::terminal_state_v2::{
    self, Anchor, Blink, Cell, Color, Cursor, CursorShape, HistoryPage, HistoryPageRequest,
    Hyperlink, Intensity, KeyboardEncoding, Modes, MouseEncoding, MouseTracking, Palette, Row,
    SCHEMA_VERSION, Screen, ScreenKind, SemanticType, State, Style, Underline, VerticalAlign,
    color,
};

#[derive(Debug)]
struct AstraTerminalConfiguration {
    scrollback_rows: usize,
    scrollback_bytes: usize,
}

impl TerminalConfiguration for AstraTerminalConfiguration {
    fn scrollback_size(&self) -> usize {
        self.scrollback_rows
    }

    fn scrollback_size_bytes(&self) -> usize {
        self.scrollback_bytes
    }

    fn color_palette(&self) -> ColorPalette {
        ColorPalette::default()
    }

    fn enable_kitty_keyboard(&self) -> bool {
        true
    }
}

/// The one authoritative VT model for an Astra PTY. It owns engine state and
/// exports bounded Terminal State v2 directly; it never renders ANSI.
pub struct TerminalEngine {
    terminal: Terminal,
    history_limits: HistoryLimits,
    epoch: [u8; 16],
    identity_epoch: (u64, u64),
    epoch_sequence_start: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HistoryLimits {
    pub rows: usize,
    pub bytes: usize,
}

impl HistoryLimits {
    pub const fn rows_only(rows: usize) -> Self {
        Self {
            rows,
            bytes: usize::MAX,
        }
    }
}

impl TerminalEngine {
    pub fn new(
        rows: u32,
        columns: u32,
        scrollback_rows: usize,
        host_reply_writer: Box<dyn Write + Send>,
    ) -> Result<Self> {
        Self::with_history_limits(
            rows,
            columns,
            HistoryLimits::rows_only(scrollback_rows),
            host_reply_writer,
        )
    }

    pub fn with_history_limits(
        rows: u32,
        columns: u32,
        history_limits: HistoryLimits,
        host_reply_writer: Box<dyn Write + Send>,
    ) -> Result<Self> {
        ensure!(
            (1..=terminal_state_v2::MAX_DIMENSION).contains(&rows)
                && (1..=terminal_state_v2::MAX_DIMENSION).contains(&columns),
            "terminal dimensions are out of range"
        );
        let config = Arc::new(AstraTerminalConfiguration {
            scrollback_rows: history_limits.rows,
            scrollback_bytes: history_limits.bytes,
        });
        let terminal = Terminal::new(
            terminal_size(rows, columns, 0, 0),
            config,
            "Astra",
            env!("CARGO_PKG_VERSION"),
            host_reply_writer,
        );
        let initial_view = terminal.astra_view();
        let identity_epoch = initial_view.identity_epoch;
        let epoch_sequence_start = initial_view.sequence;
        Ok(Self {
            terminal,
            history_limits,
            epoch: *Uuid::new_v4().as_bytes(),
            identity_epoch,
            epoch_sequence_start,
        })
    }

    pub fn advance(&mut self, bytes: &[u8]) {
        self.terminal.advance_bytes(bytes);
    }

    pub fn set_clipboard(&mut self, clipboard: &Arc<dyn Clipboard>) {
        self.terminal.set_clipboard(clipboard);
    }

    pub fn resize(
        &mut self,
        rows: u32,
        columns: u32,
        pixel_width: u32,
        pixel_height: u32,
    ) -> Result<()> {
        ensure!(
            (1..=terminal_state_v2::MAX_DIMENSION).contains(&rows)
                && (1..=terminal_state_v2::MAX_DIMENSION).contains(&columns),
            "terminal dimensions are out of range"
        );
        self.terminal
            .resize(terminal_size(rows, columns, pixel_width, pixel_height));
        Ok(())
    }

    pub fn title(&self) -> &str {
        self.terminal.get_title()
    }

    pub fn program_title(&self) -> Option<&str> {
        let view = self.terminal.astra_view();
        view.title_was_set.then_some(view.title)
    }

    pub fn semantic_state(&mut self) -> Result<State> {
        self.refresh_identity_epoch();
        let view = self.terminal.astra_view();
        validate_authoritative_screens(&view, self.history_limits)?;
        let generation = generation(&view, self.epoch_sequence_start)?;
        let mut tables = ExportTables::default();
        let alternate_row_count = view.alternate.row_count();
        ensure!(
            alternate_row_count <= terminal_state_v2::MAX_INCLUDED_ROWS,
            "alternate screen exceeds terminal state row budget"
        );
        let primary_row_budget = terminal_state_v2::MAX_INCLUDED_ROWS - alternate_row_count;
        let primary = export_screen(
            &view.primary,
            primary_row_budget,
            self.epoch_sequence_start,
            generation,
            &mut tables,
        )?;
        let alternate = export_screen(
            &view.alternate,
            alternate_row_count,
            self.epoch_sequence_start,
            generation,
            &mut tables,
        )?;
        let state = State {
            schema_version: SCHEMA_VERSION,
            epoch: self.epoch.to_vec(),
            generation,
            rows: u32::try_from(view.rows())?,
            cols: u32::try_from(view.columns())?,
            primary: Some(primary),
            alternate: Some(alternate),
            active_screen: match view.active_screen {
                AstraScreenKind::Primary => ScreenKind::Primary as i32,
                AstraScreenKind::Alternate => ScreenKind::Alternate as i32,
            },
            styles: tables.styles,
            hyperlinks: tables.hyperlinks,
            modes: Some(export_modes(view.modes)),
            title: view.title.to_owned(),
            working_directory: view.working_directory.unwrap_or_default().to_owned(),
            palette: Some(export_palette(&view.palette)),
        };
        terminal_state_v2::validate(&state).context("authoritative terminal state is invalid")?;
        Ok(state)
    }

    pub fn history_page(
        &mut self,
        request_id: u64,
        request: &HistoryPageRequest,
    ) -> Result<HistoryPage> {
        ensure!(request_id > 0, "history request ID is zero");
        terminal_state_v2::validate_history_page_request(request)?;
        self.refresh_identity_epoch();
        let view = self.terminal.astra_view();
        validate_authoritative_screens(&view, self.history_limits)?;
        let generation = generation(&view, self.epoch_sequence_start)?;
        let primary = &view.primary;
        let row_count = primary.row_count();
        let oldest_available = primary
            .rows(0, 1)
            .next()
            .map(|row| anchor(row.identity.logical_line_id, row.identity.cell_offset))
            .context("terminal screen has no oldest row")?;
        let newest_available = primary
            .rows(row_count - 1, 1)
            .next()
            .map(|row| anchor(row.identity.logical_line_id, row.identity.cell_offset))
            .context("terminal screen has no newest row")?;

        if request.epoch != self.epoch {
            let page = HistoryPage {
                request_id,
                epoch: self.epoch.to_vec(),
                generation,
                cols: u32::try_from(view.columns())?,
                included_rows: Vec::new(),
                oldest_available: Some(oldest_available),
                newest_available: Some(newest_available),
                included_start: None,
                included_end: None,
                styles: Vec::new(),
                hyperlinks: Vec::new(),
                more_before: false,
                reset_required: true,
            };
            terminal_state_v2::validate_history_page(&page)?;
            return Ok(page);
        }

        let before = request
            .before
            .as_ref()
            .context("history request anchor is missing")?;
        let before_key = (before.logical_line_id, before.cell_offset as usize);
        let end_index = primary
            .rows(0, row_count)
            .position(|row| (row.identity.logical_line_id, row.identity.cell_offset) >= before_key)
            .unwrap_or(row_count)
            .min(primary.viewport_start());
        let start_index = end_index.saturating_sub(request.maximum_rows as usize);
        let mut tables = ExportTables::default();
        let included_rows = primary
            .rows(start_index, end_index - start_index)
            .map(|row| export_row(row, self.epoch_sequence_start, generation, &mut tables))
            .collect::<Result<Vec<_>>>()?;
        let included_start = included_rows.first().and_then(|row| row.start.clone());
        let included_end = included_rows.last().and_then(|row| row.start.clone());
        let page = HistoryPage {
            request_id,
            epoch: self.epoch.to_vec(),
            generation,
            cols: u32::try_from(view.columns())?,
            included_rows,
            oldest_available: Some(oldest_available),
            newest_available: Some(newest_available),
            included_start,
            included_end,
            styles: tables.styles,
            hyperlinks: tables.hyperlinks,
            more_before: start_index > 0,
            reset_required: false,
        };
        terminal_state_v2::validate_history_page(&page)
            .context("authoritative terminal history page is invalid")?;
        Ok(page)
    }

    fn refresh_identity_epoch(&mut self) {
        let view = self.terminal.astra_view();
        if view.identity_epoch != self.identity_epoch {
            self.epoch = *Uuid::new_v4().as_bytes();
            self.identity_epoch = view.identity_epoch;
            self.epoch_sequence_start = view.sequence;
        }
    }

    #[cfg(test)]
    fn history_usage(&self) -> (usize, usize) {
        let view = self.terminal.astra_view();
        (
            view.primary.history_row_count(),
            view.primary.history_bytes(),
        )
    }

    /// Transitional serializer for clients that negotiated the registered
    /// legacy ANSI capability. This is derived from semantic state and is
    /// never fed back into the authoritative engine.
    pub fn legacy_snapshot(&mut self) -> Result<TerminalSnapshot> {
        let state = self.semantic_state()?;
        let primary = state
            .primary
            .as_ref()
            .context("primary screen is missing")?;
        let alternate = state
            .alternate
            .as_ref()
            .context("alternate screen is missing")?;
        let alternate_screen = state.active_screen == ScreenKind::Alternate as i32;
        Ok(TerminalSnapshot {
            rows: state.rows,
            cols: state.cols,
            contents: render_legacy_screen(
                &state,
                if alternate_screen { alternate } else { primary },
            ),
            alternate_screen,
            normal_contents: alternate_screen
                .then(|| render_legacy_screen(&state, primary))
                .unwrap_or_default(),
        })
    }
}

fn validate_authoritative_screens(
    view: &astra_wezterm_term::AstraTerminalView<'_>,
    history_limits: HistoryLimits,
) -> Result<()> {
    ensure!(
        view.primary.kind() == AstraScreenKind::Primary && view.primary.allows_scrollback(),
        "primary terminal screen lost scrollback semantics"
    );
    ensure!(
        view.primary.history_row_count() == view.primary.viewport_start()
            && view.primary.viewport_start() + view.rows() == view.primary.row_count(),
        "primary terminal history and viewport are not contiguous"
    );
    ensure!(
        view.primary.history_row_count() <= history_limits.rows
            && view.primary.history_bytes() <= history_limits.bytes,
        "primary terminal history exceeds its configured capacity"
    );
    ensure!(
        view.alternate.kind() == AstraScreenKind::Alternate
            && !view.alternate.allows_scrollback()
            && view.alternate.history_row_count() == 0
            && view.alternate.history_bytes() == 0
            && view.alternate.viewport_start() == 0
            && view.alternate.row_count() == view.rows(),
        "alternate terminal screen must contain exactly one viewport and no history"
    );
    Ok(())
}

fn generation(
    view: &astra_wezterm_term::AstraTerminalView<'_>,
    epoch_sequence_start: u64,
) -> Result<u64> {
    view.sequence
        .checked_sub(epoch_sequence_start)
        .context("terminal sequence moved backwards within an epoch")?
        .checked_add(1)
        .context("terminal generation overflowed")
}

fn render_legacy_screen(state: &State, screen: &Screen) -> Vec<u8> {
    let styles: BTreeMap<_, _> = state.styles.iter().map(|style| (style.id, style)).collect();
    let hyperlinks: BTreeMap<_, _> = state
        .hyperlinks
        .iter()
        .map(|link| (link.id, link))
        .collect();
    let mut output = b"\x1b[2J\x1b[H".to_vec();
    let start = screen.viewport_start as usize;
    for (row_index, row) in screen
        .included_rows
        .iter()
        .skip(start)
        .take(state.rows as usize)
        .enumerate()
    {
        for cell in &row.cells {
            output.extend_from_slice(
                format!("\x1b[{};{}H", row_index + 1, cell.column + 1).as_bytes(),
            );
            output.extend_from_slice(b"\x1b[0m");
            if let Some(style) = styles.get(&cell.style_id) {
                output.extend_from_slice(legacy_sgr(style).as_bytes());
            }
            if let Some(link) = hyperlinks.get(&cell.hyperlink_id) {
                output.extend_from_slice(b"\x1b]8;");
                if !link.explicit_id.is_empty() {
                    output.extend_from_slice(b"id=");
                    output.extend_from_slice(link.explicit_id.as_bytes());
                }
                output.push(b';');
                output.extend_from_slice(link.uri.as_bytes());
                output.extend_from_slice(b"\x1b\\");
            }
            output.extend_from_slice(cell.grapheme.as_bytes());
            if cell.hyperlink_id != 0 {
                output.extend_from_slice(b"\x1b]8;;\x1b\\");
            }
        }
    }
    output.extend_from_slice(b"\x1b[0m");
    if let Some(cursor) = &screen.cursor {
        output.extend_from_slice(format!("\x1b[{};{}H", cursor.y + 1, cursor.x + 1).as_bytes());
        output.extend_from_slice(if cursor.visible {
            b"\x1b[?25h"
        } else {
            b"\x1b[?25l"
        });
    }
    if let Some(modes) = &state.modes {
        legacy_mode(&mut output, 1, modes.application_cursor_keys);
        legacy_mode(&mut output, 5, modes.reverse_video);
        legacy_mode(&mut output, 6, modes.origin);
        legacy_mode(&mut output, 7, modes.auto_wrap);
        legacy_mode(&mut output, 45, modes.reverse_wraparound);
        legacy_mode(&mut output, 69, modes.left_right_margin);
        legacy_mode(&mut output, 1004, modes.focus_tracking);
        legacy_mode(&mut output, 1007, modes.alternate_scroll);
        legacy_mode(&mut output, 2004, modes.bracketed_paste);
        match MouseTracking::try_from(modes.mouse_tracking).unwrap_or(MouseTracking::None) {
            MouseTracking::None => {}
            MouseTracking::X10 => legacy_mode(&mut output, 9, true),
            MouseTracking::Vt200 => legacy_mode(&mut output, 1000, true),
            MouseTracking::ButtonEvent => legacy_mode(&mut output, 1002, true),
            MouseTracking::AnyEvent => legacy_mode(&mut output, 1003, true),
        }
        match MouseEncoding::try_from(modes.mouse_encoding).unwrap_or(MouseEncoding::Default) {
            MouseEncoding::Default => {}
            MouseEncoding::Utf8 => legacy_mode(&mut output, 1005, true),
            MouseEncoding::Sgr => legacy_mode(&mut output, 1006, true),
            MouseEncoding::SgrPixels => legacy_mode(&mut output, 1016, true),
        }
        output.extend_from_slice(if modes.application_keypad {
            b"\x1b="
        } else {
            b"\x1b>"
        });
    }
    output
}

fn legacy_mode(output: &mut Vec<u8>, mode: u32, enabled: bool) {
    output.extend_from_slice(format!("\x1b[?{mode}{}", if enabled { 'h' } else { 'l' }).as_bytes());
}

fn legacy_sgr(style: &Style) -> String {
    let mut codes = Vec::new();
    match Intensity::try_from(style.intensity).unwrap_or(Intensity::Normal) {
        Intensity::Normal => {}
        Intensity::Bold => codes.push("1".to_owned()),
        Intensity::Faint => codes.push("2".to_owned()),
    }
    if style.italic {
        codes.push("3".to_owned());
    }
    let underline = Underline::try_from(style.underline).unwrap_or(Underline::None);
    if underline != Underline::None {
        codes.push(
            match underline {
                Underline::Single => "4",
                Underline::Double => "4:2",
                Underline::Curly => "4:3",
                Underline::Dotted => "4:4",
                Underline::Dashed => "4:5",
                Underline::None => unreachable!(),
            }
            .to_owned(),
        );
    }
    match Blink::try_from(style.blink).unwrap_or(Blink::None) {
        Blink::None => {}
        Blink::Slow => codes.push("5".to_owned()),
        Blink::Rapid => codes.push("6".to_owned()),
    }
    if style.reverse {
        codes.push("7".to_owned());
    }
    if style.invisible {
        codes.push("8".to_owned());
    }
    if style.strikethrough {
        codes.push("9".to_owned());
    }
    if style.overline {
        codes.push("53".to_owned());
    }
    push_legacy_color(&mut codes, style.foreground.as_ref(), 38, 39);
    push_legacy_color(&mut codes, style.background.as_ref(), 48, 49);
    push_legacy_color(&mut codes, style.underline_color.as_ref(), 58, 59);
    if codes.is_empty() {
        String::new()
    } else {
        format!("\x1b[{}m", codes.join(";"))
    }
}

fn push_legacy_color(codes: &mut Vec<String>, color: Option<&Color>, prefix: u32, reset: u32) {
    match color.and_then(|color| color.value.as_ref()) {
        None | Some(color::Value::DefaultColor(_)) => codes.push(reset.to_string()),
        Some(color::Value::PaletteIndex(index)) => codes.push(format!("{prefix};5;{index}")),
        Some(color::Value::Rgb(rgb)) => codes.push(format!(
            "{prefix};2;{};{};{}",
            rgb >> 16 & 0xff,
            rgb >> 8 & 0xff,
            rgb & 0xff
        )),
    }
}

fn terminal_size(rows: u32, columns: u32, pixel_width: u32, pixel_height: u32) -> TerminalSize {
    TerminalSize {
        rows: rows as usize,
        cols: columns as usize,
        pixel_width: pixel_width as usize,
        pixel_height: pixel_height as usize,
        dpi: 0,
    }
}

fn export_screen(
    view: &AstraScreenView<'_>,
    included_row_budget: usize,
    epoch_sequence_start: u64,
    generation: u64,
    tables: &mut ExportTables,
) -> Result<Screen> {
    let row_count = view.row_count();
    ensure!(row_count > 0, "terminal engine returned an empty screen");
    let viewport_start = view.viewport_start();
    let included_start_index = row_count
        .saturating_sub(included_row_budget)
        .min(viewport_start);
    let maximum_rows = row_count - included_start_index;
    let included_rows = view
        .rows(included_start_index, maximum_rows)
        .map(|row| export_row(row, epoch_sequence_start, generation, tables))
        .collect::<Result<Vec<_>>>()?;
    let oldest_available = view
        .rows(0, 1)
        .next()
        .map(|row| anchor(row.identity.logical_line_id, row.identity.cell_offset))
        .context("terminal screen has no oldest row")?;
    let newest_available = view
        .rows(row_count - 1, 1)
        .next()
        .map(|row| anchor(row.identity.logical_line_id, row.identity.cell_offset))
        .context("terminal screen has no newest row")?;
    let included_start = included_rows
        .first()
        .and_then(|row| row.start.clone())
        .context("terminal included segment is empty")?;
    let included_end = included_rows
        .last()
        .and_then(|row| row.start.clone())
        .context("terminal included segment is empty")?;
    let included_viewport_start = viewport_start - included_start_index;
    let cursor = export_cursor(
        view.cursor,
        epoch_sequence_start,
        &included_rows,
        included_viewport_start,
    )?;
    let saved_cursor = view
        .saved_cursor
        .map(|cursor| {
            export_cursor(
                cursor,
                epoch_sequence_start,
                &included_rows,
                included_viewport_start,
            )
        })
        .transpose()?;

    Ok(Screen {
        included_rows,
        viewport_start: u32::try_from(included_viewport_start)?,
        cursor: Some(cursor),
        saved_cursor,
        oldest_available: Some(oldest_available),
        newest_available: Some(newest_available),
        included_start: Some(included_start),
        included_end: Some(included_end),
        scroll_margin_top: u32::try_from(view.scroll_margin_top)?,
        scroll_margin_bottom: u32::try_from(view.scroll_margin_bottom)?,
        scroll_margin_left: u32::try_from(view.scroll_margin_left)?,
        scroll_margin_right: u32::try_from(view.scroll_margin_right)?,
        tab_stops: view
            .tab_stops()
            .map(u32::try_from)
            .collect::<std::result::Result<Vec<_>, _>>()?,
    })
}

fn export_row(
    row: AstraRowView<'_>,
    epoch_sequence_start: u64,
    generation: u64,
    tables: &mut ExportTables,
) -> Result<Row> {
    let row_version = epoch_relative_version(row.version, epoch_sequence_start)?;
    ensure!(row_version <= generation, "row version exceeds generation");
    let wrapped_to_next = row.wrapped_to_next();
    let cells = row
        .cells()
        .filter_map(|cell| tables.export_cell(cell).transpose())
        .collect::<Result<Vec<_>>>()?;
    Ok(Row {
        start: Some(anchor(
            row.identity.logical_line_id,
            row.identity.cell_offset,
        )),
        row_version,
        cells,
        wrapped_to_next,
    })
}

fn export_cursor(
    cursor: AstraCursorView,
    epoch_sequence_start: u64,
    rows: &[Row],
    viewport_start: usize,
) -> Result<Cursor> {
    let display_x = if cursor.wrap_pending {
        cursor.x.saturating_sub(1)
    } else {
        cursor.x
    };
    let row = rows
        .get(viewport_start.saturating_add(cursor.y))
        .or_else(|| rows.get(viewport_start))
        .context("cursor is outside included viewport")?;
    let start = row.start.as_ref().context("cursor row has no anchor")?;
    Ok(Cursor {
        x: u32::try_from(display_x)?,
        y: u32::try_from(cursor.y)?,
        anchor: Some(anchor(
            start.logical_line_id,
            usize::try_from(start.cell_offset)? + display_x,
        )),
        shape: match cursor.shape {
            AstraCursorShape::Block => CursorShape::Block as i32,
            AstraCursorShape::Underline => CursorShape::Underline as i32,
            AstraCursorShape::Bar => CursorShape::Bar as i32,
        },
        visible: cursor.visible,
        version: epoch_relative_version(cursor.version, epoch_sequence_start)?,
        wrap_pending: cursor.wrap_pending,
    })
}

fn epoch_relative_version(sequence: u64, epoch_sequence_start: u64) -> Result<u64> {
    if sequence <= epoch_sequence_start {
        Ok(1)
    } else {
        sequence
            .checked_sub(epoch_sequence_start)
            .and_then(|version| version.checked_add(1))
            .context("terminal state version overflowed")
    }
}

fn anchor(logical_line_id: u64, cell_offset: usize) -> Anchor {
    Anchor {
        logical_line_id,
        cell_offset: u32::try_from(cell_offset).expect("bounded terminal cell offset fits u32"),
    }
}

fn export_modes(modes: AstraModesView) -> Modes {
    let (keyboard_encoding, keyboard_flags) = match modes.keyboard_encoding {
        AstraKeyboardEncoding::Xterm => (KeyboardEncoding::Xterm, 0),
        AstraKeyboardEncoding::CsiU => (KeyboardEncoding::CsiU, 0),
        AstraKeyboardEncoding::Kitty { flags } => (KeyboardEncoding::Kitty, flags),
    };
    Modes {
        application_cursor_keys: modes.application_cursor_keys,
        application_keypad: modes.application_keypad,
        bracketed_paste: modes.bracketed_paste,
        focus_tracking: modes.focus_tracking,
        origin: modes.origin,
        insert: modes.insert,
        auto_wrap: modes.auto_wrap,
        reverse_wraparound: modes.reverse_wraparound,
        newline: modes.newline,
        left_right_margin: modes.left_right_margin,
        reverse_video: modes.reverse_video,
        alternate_scroll: modes.alternate_scroll,
        mouse_tracking: match modes.mouse_tracking {
            AstraMouseTracking::None => MouseTracking::None as i32,
            AstraMouseTracking::X10 => MouseTracking::X10 as i32,
            AstraMouseTracking::Vt200 => MouseTracking::Vt200 as i32,
            AstraMouseTracking::ButtonEvent => MouseTracking::ButtonEvent as i32,
            AstraMouseTracking::AnyEvent => MouseTracking::AnyEvent as i32,
        },
        mouse_encoding: match modes.mouse_encoding {
            AstraMouseEncoding::Default => MouseEncoding::Default as i32,
            AstraMouseEncoding::Utf8 => MouseEncoding::Utf8 as i32,
            AstraMouseEncoding::Sgr => MouseEncoding::Sgr as i32,
            AstraMouseEncoding::SgrPixels => MouseEncoding::SgrPixels as i32,
        },
        keyboard_encoding: keyboard_encoding as i32,
        keyboard_flags,
    }
}

fn export_palette(palette: &ColorPalette) -> Palette {
    Palette {
        indexed_rgb: palette.colors.0.iter().copied().map(pack_rgb).collect(),
        foreground_rgb: pack_rgb(palette.foreground),
        background_rgb: pack_rgb(palette.background),
        cursor_fg_rgb: pack_rgb(palette.cursor_fg),
        cursor_bg_rgb: pack_rgb(palette.cursor_bg),
        selection_fg_rgb: pack_rgb(palette.selection_fg),
        selection_bg_rgb: pack_rgb(palette.selection_bg),
    }
}

fn pack_rgb(color: SrgbaTuple) -> u32 {
    let (red, green, blue, _) = color.to_srgb_u8();
    u32::from(red) << 16 | u32::from(green) << 8 | u32::from(blue)
}

#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
struct StyleKey {
    foreground: WireColor,
    background: WireColor,
    underline_color: WireColor,
    intensity: i32,
    underline: i32,
    blink: i32,
    italic: bool,
    reverse: bool,
    strikethrough: bool,
    invisible: bool,
    overline: bool,
    semantic_type: i32,
    vertical_align: i32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
enum WireColor {
    #[default]
    Default,
    Palette(u8),
    Rgb(u32),
}

#[derive(Default)]
struct ExportTables {
    style_ids: BTreeMap<StyleKey, u32>,
    styles: Vec<Style>,
    hyperlink_ids: BTreeMap<(String, String), u64>,
    hyperlinks: Vec<Hyperlink>,
}

impl ExportTables {
    fn export_cell(&mut self, cell: AstraCellView<'_>) -> Result<Option<Cell>> {
        let style_key = style_key(cell.attributes());
        let style_id = self.style_id(style_key)?;
        let hyperlink_id = self.hyperlink_id(cell.attributes())?;
        if cell.grapheme() == " " && style_id == 0 && hyperlink_id == 0 {
            return Ok(None);
        }
        Ok(Some(Cell {
            column: u32::try_from(cell.column())?,
            grapheme: cell.grapheme().to_owned(),
            width: u32::try_from(cell.width())?,
            style_id,
            hyperlink_id,
        }))
    }

    fn style_id(&mut self, key: StyleKey) -> Result<u32> {
        if key == StyleKey::default() {
            return Ok(0);
        }
        if let Some(id) = self.style_ids.get(&key) {
            return Ok(*id);
        }
        let id = u32::try_from(self.styles.len() + 1)?;
        ensure!(
            self.styles.len() < terminal_state_v2::MAX_STYLES,
            "terminal style table is full"
        );
        self.styles.push(Style {
            id,
            foreground: Some(export_color(&key.foreground)),
            background: Some(export_color(&key.background)),
            underline_color: Some(export_color(&key.underline_color)),
            intensity: key.intensity,
            underline: key.underline,
            blink: key.blink,
            italic: key.italic,
            reverse: key.reverse,
            strikethrough: key.strikethrough,
            invisible: key.invisible,
            overline: key.overline,
            semantic_type: key.semantic_type,
            vertical_align: key.vertical_align,
        });
        self.style_ids.insert(key, id);
        Ok(id)
    }

    fn hyperlink_id(&mut self, attributes: &CellAttributes) -> Result<u64> {
        let Some(link) = attributes.hyperlink() else {
            return Ok(0);
        };
        let explicit_id = link.params().get("id").cloned().unwrap_or_default();
        let key = (link.uri().to_owned(), explicit_id.clone());
        if let Some(id) = self.hyperlink_ids.get(&key) {
            return Ok(*id);
        }
        ensure!(
            self.hyperlinks.len() < terminal_state_v2::MAX_HYPERLINKS,
            "terminal hyperlink table is full"
        );
        let id = u64::try_from(self.hyperlinks.len() + 1)?;
        self.hyperlinks.push(Hyperlink {
            id,
            uri: key.0.clone(),
            explicit_id,
        });
        self.hyperlink_ids.insert(key, id);
        Ok(id)
    }
}

fn style_key(attributes: &CellAttributes) -> StyleKey {
    StyleKey {
        foreground: wire_color(attributes.foreground()),
        background: wire_color(attributes.background()),
        underline_color: wire_color(attributes.underline_color()),
        intensity: match attributes.intensity() {
            astra_wezterm_term::Intensity::Normal => Intensity::Normal as i32,
            astra_wezterm_term::Intensity::Bold => Intensity::Bold as i32,
            astra_wezterm_term::Intensity::Half => Intensity::Faint as i32,
        },
        underline: match attributes.underline() {
            astra_wezterm_term::Underline::None => Underline::None as i32,
            astra_wezterm_term::Underline::Single => Underline::Single as i32,
            astra_wezterm_term::Underline::Double => Underline::Double as i32,
            astra_wezterm_term::Underline::Curly => Underline::Curly as i32,
            astra_wezterm_term::Underline::Dotted => Underline::Dotted as i32,
            astra_wezterm_term::Underline::Dashed => Underline::Dashed as i32,
        },
        blink: match attributes.blink() {
            astra_wezterm_term::Blink::None => Blink::None as i32,
            astra_wezterm_term::Blink::Slow => Blink::Slow as i32,
            astra_wezterm_term::Blink::Rapid => Blink::Rapid as i32,
        },
        italic: attributes.italic(),
        reverse: attributes.reverse(),
        strikethrough: attributes.strikethrough(),
        invisible: attributes.invisible(),
        overline: attributes.overline(),
        semantic_type: match attributes.semantic_type() {
            astra_wezterm_term::SemanticType::Output => SemanticType::Output as i32,
            astra_wezterm_term::SemanticType::Input => SemanticType::Input as i32,
            astra_wezterm_term::SemanticType::Prompt => SemanticType::Prompt as i32,
        },
        vertical_align: match attributes.vertical_align() {
            astra_wezterm_term::VerticalAlign::BaseLine => VerticalAlign::Baseline as i32,
            astra_wezterm_term::VerticalAlign::SuperScript => VerticalAlign::Superscript as i32,
            astra_wezterm_term::VerticalAlign::SubScript => VerticalAlign::Subscript as i32,
        },
    }
}

fn wire_color(color: ColorAttribute) -> WireColor {
    match color {
        ColorAttribute::Default => WireColor::Default,
        ColorAttribute::PaletteIndex(index) => WireColor::Palette(index),
        ColorAttribute::TrueColorWithPaletteFallback(color, _)
        | ColorAttribute::TrueColorWithDefaultFallback(color) => WireColor::Rgb(pack_rgb(color)),
    }
}

fn export_color(color: &WireColor) -> Color {
    Color {
        value: Some(match color {
            WireColor::Default => color::Value::DefaultColor(true),
            WireColor::Palette(index) => color::Value::PaletteIndex(u32::from(*index)),
            WireColor::Rgb(rgb) => color::Value::Rgb(*rgb),
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crate::resources::ResourcePolicy;

    use super::*;

    #[derive(Clone, Default)]
    struct ReplySink(Arc<Mutex<Vec<u8>>>);

    impl Write for ReplySink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn row_text(row: &Row) -> String {
        row.cells
            .iter()
            .map(|cell| cell.grapheme.as_str())
            .collect()
    }

    fn logical_text(screen: &Screen, logical_line_id: u64) -> String {
        screen
            .included_rows
            .iter()
            .filter(|row| {
                row.start
                    .as_ref()
                    .is_some_and(|anchor| anchor.logical_line_id == logical_line_id)
            })
            .map(row_text)
            .collect()
    }

    fn assert_same_row_content_and_identity(left: &[Row], right: &[Row]) {
        assert_eq!(left.len(), right.len());
        for (left, right) in left.iter().zip(right) {
            assert_eq!(left.start, right.start);
            assert_eq!(left.cells, right.cells);
            assert_eq!(left.wrapped_to_next, right.wrapped_to_next);
        }
    }

    #[test]
    fn exports_both_screens_styles_modes_and_hyperlinks_without_ansi() {
        let sink = ReplySink::default();
        let mut engine = TerminalEngine::new(3, 12, 32, Box::new(sink)).unwrap();
        engine.advance(
            b"\x1b[31;1mred\x1b[0m \x1b]8;id=docs;https://example.com\x1b\\link\x1b]8;;\x1b\\",
        );
        engine.advance(b"\x1b[?1049h\x1b[?1006h\x1b[?1000hALT");

        let state = engine.semantic_state().unwrap();
        assert_eq!(state.active_screen, ScreenKind::Alternate as i32);
        assert!(!state.primary.as_ref().unwrap().included_rows.is_empty());
        assert_eq!(state.alternate.as_ref().unwrap().included_rows.len(), 3);
        assert!(!state.styles.is_empty());
        assert_eq!(state.hyperlinks.len(), 1);
        assert_eq!(
            state.modes.as_ref().unwrap().mouse_encoding,
            MouseEncoding::Sgr as i32
        );
        assert!(
            state
                .primary
                .as_ref()
                .unwrap()
                .included_rows
                .iter()
                .flat_map(|row| &row.cells)
                .any(|cell| cell.grapheme == "red" || cell.grapheme == "r")
        );
    }

    #[test]
    fn exports_authoritative_input_modes_including_x10_and_alternate_scroll() {
        let sink = ReplySink::default();
        let mut engine = TerminalEngine::new(3, 12, 32, Box::new(sink)).unwrap();

        engine.advance(b"\x1b[?1h\x1b=\x1b[?9h\x1b[?1006h\x1b[?1007l\x1b[?2004h\x1b[=5u");
        let state = engine.semantic_state().unwrap();
        let modes = state.modes.unwrap();

        assert!(modes.application_cursor_keys);
        assert!(modes.application_keypad);
        assert!(modes.bracketed_paste);
        assert!(!modes.alternate_scroll);
        assert_eq!(modes.mouse_tracking, MouseTracking::X10 as i32);
        assert_eq!(modes.mouse_encoding, MouseEncoding::Sgr as i32);
        assert_eq!(modes.keyboard_encoding, KeyboardEncoding::Kitty as i32);
        assert_eq!(modes.keyboard_flags, 5);

        engine.advance(b"\x1b[?9l\x1b[?1003h\x1b[?1007h");
        let modes = engine.semantic_state().unwrap().modes.unwrap();
        assert!(modes.alternate_scroll);
        assert_eq!(modes.mouse_tracking, MouseTracking::AnyEvent as i32);
    }

    #[test]
    fn logical_line_id_survives_resize_reflow() {
        let mut engine = TerminalEngine::new(3, 8, 32, Box::new(ReplySink::default())).unwrap();
        engine.advance(b"abcdefghijklmno");
        let before = engine.semantic_state().unwrap();
        let before_rows = &before.primary.as_ref().unwrap().included_rows;
        let logical_id = before_rows
            .iter()
            .find(|row| row.wrapped_to_next)
            .unwrap()
            .start
            .as_ref()
            .unwrap()
            .logical_line_id;

        engine.resize(3, 5, 0, 0).unwrap();
        let after = engine.semantic_state().unwrap();
        let after_rows = &after.primary.as_ref().unwrap().included_rows;
        assert!(
            after_rows
                .iter()
                .filter(|row| { row.start.as_ref().unwrap().logical_line_id == logical_id })
                .count()
                >= 2
        );
        assert_eq!(before.epoch, after.epoch);
    }

    #[test]
    fn primary_history_preserves_cells_styles_hyperlinks_and_soft_wrap_identity() {
        let mut engine = TerminalEngine::new(3, 6, 32, Box::new(ReplySink::default())).unwrap();
        engine.advance(b"\x1b[31m\x1b]8;;https://example.com/history\x1b\\abc");
        engine.advance("界".as_bytes());
        engine.advance(b"def\x1b]8;;\x1b\\\x1b[0m\r\nhard-1\r\nhard-2\r\ntail");

        let state = engine.semantic_state().unwrap();
        let primary = state.primary.as_ref().unwrap();
        assert!(
            primary.viewport_start > 0,
            "test output did not enter history"
        );
        let history = &primary.included_rows[..primary.viewport_start as usize];
        let wide = history
            .iter()
            .flat_map(|row| &row.cells)
            .find(|cell| cell.grapheme == "界")
            .expect("wide linked cell was lost from history");
        assert_eq!(wide.width, 2);
        assert_ne!(wide.style_id, 0);
        assert_ne!(wide.hyperlink_id, 0);
        assert!(state.styles.iter().any(|style| style.id == wide.style_id));
        assert!(
            state
                .hyperlinks
                .iter()
                .any(|link| link.id == wide.hyperlink_id
                    && link.uri == "https://example.com/history")
        );

        let linked_rows: Vec<_> = primary
            .included_rows
            .iter()
            .filter(|row| row.cells.iter().any(|cell| cell.hyperlink_id != 0))
            .collect();
        assert!(
            linked_rows.len() >= 2,
            "linked logical line did not soft-wrap"
        );
        let first = linked_rows[0].start.as_ref().unwrap();
        let second = linked_rows[1].start.as_ref().unwrap();
        assert!(linked_rows[0].wrapped_to_next);
        assert_eq!(first.logical_line_id, second.logical_line_id);
        assert_eq!(second.cell_offset, first.cell_offset + state.cols);
        let following_hard_line = primary
            .included_rows
            .iter()
            .skip_while(|row| row.start.as_ref().unwrap().logical_line_id == first.logical_line_id)
            .next()
            .unwrap();
        assert!(
            following_hard_line.start.as_ref().unwrap().logical_line_id > first.logical_line_id
        );
    }

    #[test]
    fn repeated_reflow_preserves_logical_history_identity_and_content() {
        let mut engine = TerminalEngine::new(4, 8, 64, Box::new(ReplySink::default())).unwrap();
        engine.advance(b"abcdefghijklmno\r\nsecond\r\nthird\r\nfourth\r\ntail");
        let initial = engine.semantic_state().unwrap();
        let initial_primary = initial.primary.as_ref().unwrap();
        let logical_id = initial_primary
            .included_rows
            .iter()
            .find(|row| row.wrapped_to_next && row_text(row).starts_with("abcdefgh"))
            .unwrap()
            .start
            .as_ref()
            .unwrap()
            .logical_line_id;
        assert_eq!(logical_text(initial_primary, logical_id), "abcdefghijklmno");

        engine.resize(4, 5, 0, 0).unwrap();
        let narrow = engine.semantic_state().unwrap();
        assert_eq!(narrow.epoch, initial.epoch);
        assert_eq!(
            logical_text(narrow.primary.as_ref().unwrap(), logical_id),
            "abcdefghijklmno"
        );
        let narrow_offsets: Vec<_> = narrow
            .primary
            .as_ref()
            .unwrap()
            .included_rows
            .iter()
            .filter_map(|row| row.start.as_ref())
            .filter(|anchor| anchor.logical_line_id == logical_id)
            .map(|anchor| anchor.cell_offset)
            .collect();
        assert_eq!(narrow_offsets, vec![0, 5, 10]);

        engine.resize(4, 12, 0, 0).unwrap();
        let wide = engine.semantic_state().unwrap();
        assert_eq!(wide.epoch, initial.epoch);
        assert_eq!(
            logical_text(wide.primary.as_ref().unwrap(), logical_id),
            "abcdefghijklmno"
        );
        let wide_offsets: Vec<_> = wide
            .primary
            .as_ref()
            .unwrap()
            .included_rows
            .iter()
            .filter_map(|row| row.start.as_ref())
            .filter(|anchor| anchor.logical_line_id == logical_id)
            .map(|anchor| anchor.cell_offset)
            .collect();
        assert_eq!(wide_offsets, vec![0, 12]);
    }

    #[test]
    fn normal_history_trim_advances_oldest_anchor_without_reusing_ids() {
        let mut engine = TerminalEngine::new(2, 8, 2, Box::new(ReplySink::default())).unwrap();
        engine.advance(b"line-0\r\nline-1\r\nline-2\r\nline-3\r\nline-4");
        let before = engine.semantic_state().unwrap();
        let before_primary = before.primary.as_ref().unwrap();
        let before_oldest = before_primary
            .oldest_available
            .as_ref()
            .unwrap()
            .logical_line_id;
        let before_newest = before_primary
            .newest_available
            .as_ref()
            .unwrap()
            .logical_line_id;

        engine.advance(b"\r\nline-5\r\nline-6\r\nline-7");
        let after = engine.semantic_state().unwrap();
        let after_primary = after.primary.as_ref().unwrap();
        let after_oldest = after_primary
            .oldest_available
            .as_ref()
            .unwrap()
            .logical_line_id;
        let after_newest = after_primary
            .newest_available
            .as_ref()
            .unwrap()
            .logical_line_id;
        assert_eq!(after.epoch, before.epoch);
        assert!(after_oldest > before_oldest);
        assert!(after_newest > before_newest);
        assert!(
            after_primary.included_rows.iter().all(|row| row
                .start
                .as_ref()
                .unwrap()
                .logical_line_id
                >= after_oldest)
        );
        assert!(
            after_oldest <= before_newest,
            "retained rows should keep their IDs"
        );
    }

    #[test]
    fn accounted_byte_limit_trims_complex_history_before_the_row_limit() {
        let byte_limit = 4 * 1024;
        let mut engine = TerminalEngine::with_history_limits(
            2,
            40,
            HistoryLimits {
                rows: 100,
                bytes: byte_limit,
            },
            Box::new(ReplySink::default()),
        )
        .unwrap();
        let uri = format!("https://example.com/{}", "history".repeat(32));
        let mut output = Vec::new();
        for index in 0..40 {
            output.extend_from_slice(
                format!("\x1b]8;;{uri}\x1b\\linked-{index}\x1b]8;;\x1b\\\r\n").as_bytes(),
            );
        }
        engine.advance(&output);

        let (history_rows, history_bytes) = engine.history_usage();
        assert!(history_rows > 0);
        assert!(history_rows < 38, "byte limit did not trim history");
        assert!(history_bytes <= byte_limit);
        let state = engine.semantic_state().unwrap();
        let primary = state.primary.as_ref().unwrap();
        assert_eq!(primary.viewport_start as usize, history_rows);
        assert!(primary.oldest_available.as_ref().unwrap().logical_line_id > 1);
    }

    #[test]
    fn default_capacity_retains_ten_thousand_simple_rows() {
        let policy = ResourcePolicy::default();
        let limits = HistoryLimits {
            rows: usize::try_from(policy.terminal_history_rows).unwrap(),
            bytes: usize::try_from(policy.terminal_history_bytes).unwrap(),
        };
        let mut engine =
            TerminalEngine::with_history_limits(2, 8, limits, Box::new(ReplySink::default()))
                .unwrap();
        engine.advance("x\r\n".repeat(10_005).as_bytes());

        let (history_rows, history_bytes) = engine.history_usage();
        assert_eq!(history_rows, 10_000);
        assert!(history_bytes <= limits.bytes);
        let before = engine.semantic_state().unwrap();
        let before_oldest = before
            .primary
            .as_ref()
            .unwrap()
            .oldest_available
            .as_ref()
            .unwrap()
            .logical_line_id;

        engine.advance(b"y\r\n");
        assert_eq!(engine.history_usage().0, 10_000);
        let after = engine.semantic_state().unwrap();
        let after_oldest = after
            .primary
            .as_ref()
            .unwrap()
            .oldest_available
            .as_ref()
            .unwrap()
            .logical_line_id;
        assert!(after_oldest > before_oldest);
    }

    #[test]
    fn byte_accounting_is_recomputed_after_reflow() {
        let byte_limit = 16 * 1024;
        let mut engine = TerminalEngine::with_history_limits(
            3,
            7,
            HistoryLimits {
                rows: 100,
                bytes: byte_limit,
            },
            Box::new(ReplySink::default()),
        )
        .unwrap();
        let mut output = Vec::new();
        for index in 0..30 {
            output.extend_from_slice(format!("r{index:02}abcdef\r\n").as_bytes());
        }
        engine.advance(&output);
        let before = engine.semantic_state().unwrap();
        let before_oldest = before
            .primary
            .as_ref()
            .unwrap()
            .oldest_available
            .clone()
            .unwrap();

        engine.resize(3, 4, 0, 0).unwrap();
        let (history_rows, history_bytes) = engine.history_usage();
        assert!(history_rows <= 100);
        assert!(history_bytes <= byte_limit);
        let after = engine.semantic_state().unwrap();
        let after_oldest = after
            .primary
            .as_ref()
            .unwrap()
            .oldest_available
            .as_ref()
            .unwrap();
        assert_eq!(after.epoch, before.epoch);
        assert!(
            (after_oldest.logical_line_id, after_oldest.cell_offset)
                >= (before_oldest.logical_line_id, before_oldest.cell_offset)
        );
    }

    #[test]
    fn reflow_trim_advances_only_the_removed_soft_wrap_prefix() {
        let mut engine = TerminalEngine::new(2, 5, 2, Box::new(ReplySink::default())).unwrap();
        engine.advance(b"abcdefghijklmnopqrstuvwxyz1234");

        let before = engine.semantic_state().unwrap();
        let before_primary = before.primary.as_ref().unwrap();
        let first_before = before_primary.included_rows[0].start.as_ref().unwrap();
        let logical_id = first_before.logical_line_id;
        let retained_offset = first_before.cell_offset;
        let retained_text = logical_text(before_primary, logical_id);
        assert!(retained_offset > 0, "test did not trim the logical prefix");
        assert!(
            before_primary.included_rows.iter().all(|row| row
                .start
                .as_ref()
                .unwrap()
                .logical_line_id
                == logical_id)
        );

        engine.resize(2, 4, 0, 0).unwrap();
        let after = engine.semantic_state().unwrap();
        let after_primary = after.primary.as_ref().unwrap();
        let first_after = after_primary.included_rows[0].start.as_ref().unwrap();
        assert_eq!(after.epoch, before.epoch);
        assert_eq!(first_after.logical_line_id, logical_id);
        assert!(first_after.cell_offset >= retained_offset);
        let after_text = logical_text(after_primary, logical_id);
        assert!(retained_text.ends_with(&after_text));

        let offsets: Vec<_> = after_primary
            .included_rows
            .iter()
            .map(|row| row.start.as_ref().unwrap().cell_offset)
            .collect();
        assert!(
            offsets
                .windows(2)
                .all(|pair| pair[1] == pair[0] + after.cols)
        );
    }

    #[test]
    fn alternate_screen_never_creates_history_or_mutates_primary_history() {
        let mut engine = TerminalEngine::new(3, 8, 16, Box::new(ReplySink::default())).unwrap();
        engine.advance(b"one\r\ntwo\r\nthree\r\nfour\r\nfive");
        let before = engine.semantic_state().unwrap();
        let primary_before = before.primary.as_ref().unwrap().included_rows.clone();
        assert!(before.primary.as_ref().unwrap().viewport_start > 0);

        engine.advance(b"\x1b[?1049hA\r\nB\r\nC\r\nD\r\nE\r\nF");
        let alternate = engine.semantic_state().unwrap();
        let alternate_screen = alternate.alternate.as_ref().unwrap();
        assert_eq!(alternate.active_screen, ScreenKind::Alternate as i32);
        assert_eq!(alternate_screen.viewport_start, 0);
        assert_eq!(
            alternate_screen.included_rows.len(),
            alternate.rows as usize
        );
        assert_eq!(
            alternate_screen.oldest_available,
            alternate_screen.included_start
        );
        assert_eq!(
            alternate_screen.newest_available,
            alternate_screen.included_end
        );
        assert_same_row_content_and_identity(
            &alternate.primary.as_ref().unwrap().included_rows,
            &primary_before,
        );

        engine.advance(b"\x1b[?1049l");
        let restored = engine.semantic_state().unwrap();
        assert_eq!(restored.active_screen, ScreenKind::Primary as i32);
        assert_same_row_content_and_identity(
            &restored.primary.as_ref().unwrap().included_rows,
            &primary_before,
        );
    }

    #[test]
    fn history_pages_are_contiguous_bounded_and_epoch_scoped() {
        let mut engine = TerminalEngine::new(2, 8, 32, Box::new(ReplySink::default())).unwrap();
        engine.advance(b"line-0\r\nline-1\r\nline-2\r\nline-3\r\nline-4\r\nline-5");
        let state = engine.semantic_state().unwrap();
        let primary = state.primary.as_ref().unwrap();
        let before = primary.included_rows[primary.viewport_start as usize]
            .start
            .clone();
        let request = HistoryPageRequest {
            epoch: state.epoch.clone(),
            before,
            maximum_rows: 2,
        };
        let page = engine.history_page(7, &request).unwrap();
        assert_eq!(page.request_id, 7);
        assert_eq!(page.epoch, state.epoch);
        assert_eq!(page.cols, state.cols);
        assert_eq!(page.included_rows.len(), 2);
        assert!(page.more_before);
        assert!(!page.reset_required);
        let page_end = page.included_end.as_ref().unwrap();
        let request_before = request.before.as_ref().unwrap();
        assert!(
            (page_end.logical_line_id, page_end.cell_offset)
                < (request_before.logical_line_id, request_before.cell_offset)
        );

        let oldest_request = HistoryPageRequest {
            before: page.oldest_available.clone(),
            ..request.clone()
        };
        let empty = engine.history_page(8, &oldest_request).unwrap();
        assert!(empty.included_rows.is_empty());
        assert!(!empty.more_before);

        engine.advance(b"\x1b[2;1H\x1b[L");
        let reset = engine.history_page(9, &request).unwrap();
        assert!(reset.reset_required);
        assert!(reset.included_rows.is_empty());
        assert_ne!(reset.epoch, request.epoch);
    }

    #[test]
    fn structural_row_insertion_rotates_anchor_epoch_and_restarts_generation() {
        let mut engine = TerminalEngine::new(4, 12, 32, Box::new(ReplySink::default())).unwrap();
        engine.advance(b"one\r\ntwo\r\nthree");
        let before = engine.semantic_state().unwrap();
        assert!(before.generation > 1);

        // IL inserts a row above retained rows. Keeping their old numeric IDs
        // would violate anchor ordering, so the fork rebases identity space.
        engine.advance(b"\x1b[2;1H\x1b[L");
        let rebased = engine.semantic_state().unwrap();
        assert_ne!(before.epoch, rebased.epoch);
        assert_eq!(rebased.generation, 1);
        assert!(
            rebased
                .primary
                .as_ref()
                .unwrap()
                .included_rows
                .windows(2)
                .all(|rows| {
                    let left = rows[0].start.as_ref().unwrap();
                    let right = rows[1].start.as_ref().unwrap();
                    (left.logical_line_id, left.cell_offset)
                        < (right.logical_line_id, right.cell_offset)
                })
        );

        engine.advance(b"replacement");
        let changed = engine.semantic_state().unwrap();
        assert_eq!(rebased.epoch, changed.epoch);
        assert!(changed.generation > rebased.generation);
    }

    #[test]
    fn semantic_export_budgets_primary_history_and_alternate_rows_together() {
        let mut engine =
            TerminalEngine::new(100, 4, 5_000, Box::new(ReplySink::default())).unwrap();
        let output = b"x\r\n".repeat(4_100);
        engine.advance(&output);

        let state = engine.semantic_state().unwrap();
        let primary = state.primary.as_ref().unwrap();
        let alternate = state.alternate.as_ref().unwrap();
        assert_eq!(alternate.included_rows.len(), 100);
        assert_eq!(
            primary.included_rows.len() + alternate.included_rows.len(),
            terminal_state_v2::MAX_INCLUDED_ROWS
        );
        assert_eq!(
            primary.included_rows.len() - primary.viewport_start as usize,
            state.rows as usize
        );
    }

    #[test]
    fn terminal_query_replies_are_sent_to_host_sink() {
        let sink = ReplySink::default();
        let captured = sink.0.clone();
        let mut engine = TerminalEngine::new(24, 80, 0, Box::new(sink)).unwrap();
        engine.advance(b"\x1b[6n");

        for _ in 0..100 {
            if !captured.lock().unwrap().is_empty() {
                break;
            }
            std::thread::yield_now();
        }
        assert_eq!(&*captured.lock().unwrap(), b"\x1b[1;1R");
    }

    #[test]
    fn terminal_capability_queries_report_real_geometry_and_deny_private_data() {
        let sink = ReplySink::default();
        let captured = sink.0.clone();
        let mut engine = TerminalEngine::new(24, 80, 0, Box::new(sink)).unwrap();
        engine.resize(30, 100, 1000, 600).unwrap();
        engine.advance(b"\x1b[c\x1b[5n\x1b[18t\x1b[16t\x1b[14t\x1b[21t\x1b]52;c;?\x07");

        for _ in 0..100 {
            if captured.lock().unwrap().len() >= 30 {
                break;
            }
            std::thread::yield_now();
        }
        let replies = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
        assert!(replies.contains("\x1b[?65;6;18;22c"), "{replies:?}");
        assert!(!replies.contains(";52c"), "{replies:?}");
        assert!(replies.contains("\x1b[0n"), "{replies:?}");
        assert!(replies.contains("\x1b[8;30;100t"), "{replies:?}");
        assert!(replies.contains("\x1b[6;20;10t"), "{replies:?}");
        assert!(replies.contains("\x1b[4;600;1000t"), "{replies:?}");
        assert!(
            !replies.contains("]l"),
            "title reporting must remain disabled"
        );
        assert!(
            !replies.contains("]52;"),
            "clipboard reads must never be answered"
        );
    }

    #[test]
    fn pixel_queries_safely_degrade_when_the_client_cannot_measure_pixels() {
        let sink = ReplySink::default();
        let captured = sink.0.clone();
        let mut engine = TerminalEngine::new(24, 80, 0, Box::new(sink)).unwrap();
        engine.advance(b"\x1b[16t\x1b[14t");

        for _ in 0..10 {
            std::thread::yield_now();
        }
        assert!(captured.lock().unwrap().is_empty());
    }
}
