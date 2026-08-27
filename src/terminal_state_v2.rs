use std::collections::HashSet;

use anyhow::{Result, ensure};
use prost::Message;
use unicode_segmentation::UnicodeSegmentation;

pub const SCHEMA_VERSION: u32 = 2;
pub const EPOCH_BYTES: usize = 16;
pub const MAX_ENCODED_STATE_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_ENCODED_HISTORY_PAGE_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_DIMENSION: u32 = 1_000;
pub const MAX_INCLUDED_ROWS: usize = 4_096;
pub const MAX_HISTORY_PAGE_ROWS: usize = 512;
pub const MAX_CELLS: usize = 1_000_000;
pub const MAX_GRAPHEME_BYTES: usize = 256;
pub const MAX_STYLES: usize = 4_096;
pub const MAX_HYPERLINKS: usize = 4_096;
pub const MAX_HYPERLINK_URI_BYTES: usize = 16 * 1024;
pub const MAX_HYPERLINK_EXPLICIT_ID_BYTES: usize = 1_024;
pub const MAX_TITLE_BYTES: usize = 512;
pub const MAX_WORKING_DIRECTORY_BYTES: usize = 16 * 1024;

#[derive(Clone, PartialEq, Message)]
pub struct State {
    #[prost(uint32, tag = "1")]
    pub schema_version: u32,
    #[prost(bytes = "vec", tag = "2")]
    pub epoch: Vec<u8>,
    #[prost(uint64, tag = "3")]
    pub generation: u64,
    #[prost(uint32, tag = "4")]
    pub rows: u32,
    #[prost(uint32, tag = "5")]
    pub cols: u32,
    #[prost(message, optional, tag = "6")]
    pub primary: Option<Screen>,
    #[prost(message, optional, tag = "7")]
    pub alternate: Option<Screen>,
    #[prost(enumeration = "ScreenKind", tag = "8")]
    pub active_screen: i32,
    #[prost(message, repeated, tag = "9")]
    pub styles: Vec<Style>,
    #[prost(message, repeated, tag = "10")]
    pub hyperlinks: Vec<Hyperlink>,
    #[prost(message, optional, tag = "11")]
    pub modes: Option<Modes>,
    #[prost(string, tag = "12")]
    pub title: String,
    #[prost(string, tag = "13")]
    pub working_directory: String,
    #[prost(message, optional, tag = "14")]
    pub palette: Option<Palette>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
#[repr(i32)]
pub enum ScreenKind {
    Unspecified = 0,
    Primary = 1,
    Alternate = 2,
}

#[derive(Clone, PartialEq, Message)]
pub struct Screen {
    #[prost(message, repeated, tag = "1")]
    pub included_rows: Vec<Row>,
    #[prost(uint32, tag = "2")]
    pub viewport_start: u32,
    #[prost(message, optional, tag = "3")]
    pub cursor: Option<Cursor>,
    #[prost(message, optional, tag = "4")]
    pub saved_cursor: Option<Cursor>,
    #[prost(message, optional, tag = "5")]
    pub oldest_available: Option<Anchor>,
    #[prost(message, optional, tag = "6")]
    pub newest_available: Option<Anchor>,
    #[prost(message, optional, tag = "7")]
    pub included_start: Option<Anchor>,
    #[prost(message, optional, tag = "8")]
    pub included_end: Option<Anchor>,
    #[prost(uint32, tag = "9")]
    pub scroll_margin_top: u32,
    #[prost(uint32, tag = "10")]
    pub scroll_margin_bottom: u32,
    #[prost(uint32, tag = "11")]
    pub scroll_margin_left: u32,
    #[prost(uint32, tag = "12")]
    pub scroll_margin_right: u32,
    #[prost(uint32, repeated, packed = "true", tag = "13")]
    pub tab_stops: Vec<u32>,
}

#[derive(Clone, PartialEq, Eq, Message)]
pub struct Anchor {
    #[prost(uint64, tag = "1")]
    pub logical_line_id: u64,
    #[prost(uint32, tag = "2")]
    pub cell_offset: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct HistoryPageRequest {
    #[prost(bytes = "vec", tag = "1")]
    pub epoch: Vec<u8>,
    #[prost(message, optional, tag = "2")]
    pub before: Option<Anchor>,
    #[prost(uint32, tag = "3")]
    pub maximum_rows: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct HistoryPage {
    #[prost(uint64, tag = "1")]
    pub request_id: u64,
    #[prost(bytes = "vec", tag = "2")]
    pub epoch: Vec<u8>,
    #[prost(uint64, tag = "3")]
    pub generation: u64,
    #[prost(uint32, tag = "4")]
    pub cols: u32,
    #[prost(message, repeated, tag = "5")]
    pub included_rows: Vec<Row>,
    #[prost(message, optional, tag = "6")]
    pub oldest_available: Option<Anchor>,
    #[prost(message, optional, tag = "7")]
    pub newest_available: Option<Anchor>,
    #[prost(message, optional, tag = "8")]
    pub included_start: Option<Anchor>,
    #[prost(message, optional, tag = "9")]
    pub included_end: Option<Anchor>,
    #[prost(message, repeated, tag = "10")]
    pub styles: Vec<Style>,
    #[prost(message, repeated, tag = "11")]
    pub hyperlinks: Vec<Hyperlink>,
    #[prost(bool, tag = "12")]
    pub more_before: bool,
    #[prost(bool, tag = "13")]
    pub reset_required: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct Row {
    #[prost(message, optional, tag = "1")]
    pub start: Option<Anchor>,
    #[prost(uint64, tag = "2")]
    pub row_version: u64,
    #[prost(message, repeated, tag = "3")]
    pub cells: Vec<Cell>,
    #[prost(bool, tag = "4")]
    pub wrapped_to_next: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct Cell {
    #[prost(uint32, tag = "1")]
    pub column: u32,
    #[prost(string, tag = "2")]
    pub grapheme: String,
    #[prost(uint32, tag = "3")]
    pub width: u32,
    #[prost(uint32, tag = "4")]
    pub style_id: u32,
    #[prost(uint64, tag = "5")]
    pub hyperlink_id: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct Cursor {
    #[prost(uint32, tag = "1")]
    pub x: u32,
    #[prost(uint32, tag = "2")]
    pub y: u32,
    #[prost(message, optional, tag = "3")]
    pub anchor: Option<Anchor>,
    #[prost(enumeration = "CursorShape", tag = "4")]
    pub shape: i32,
    #[prost(bool, tag = "5")]
    pub visible: bool,
    #[prost(uint64, tag = "6")]
    pub version: u64,
    #[prost(bool, tag = "7")]
    pub wrap_pending: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
#[repr(i32)]
pub enum CursorShape {
    Unspecified = 0,
    Block = 1,
    Underline = 2,
    Bar = 3,
}

#[derive(Clone, PartialEq, Message)]
pub struct Style {
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(message, optional, tag = "2")]
    pub foreground: Option<Color>,
    #[prost(message, optional, tag = "3")]
    pub background: Option<Color>,
    #[prost(message, optional, tag = "4")]
    pub underline_color: Option<Color>,
    #[prost(enumeration = "Intensity", tag = "5")]
    pub intensity: i32,
    #[prost(enumeration = "Underline", tag = "6")]
    pub underline: i32,
    #[prost(enumeration = "Blink", tag = "7")]
    pub blink: i32,
    #[prost(bool, tag = "8")]
    pub italic: bool,
    #[prost(bool, tag = "9")]
    pub reverse: bool,
    #[prost(bool, tag = "10")]
    pub strikethrough: bool,
    #[prost(bool, tag = "11")]
    pub invisible: bool,
    #[prost(bool, tag = "12")]
    pub overline: bool,
    #[prost(enumeration = "SemanticType", tag = "13")]
    pub semantic_type: i32,
    #[prost(enumeration = "VerticalAlign", tag = "14")]
    pub vertical_align: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct Color {
    #[prost(oneof = "color::Value", tags = "1, 2, 3")]
    pub value: Option<color::Value>,
}

pub mod color {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Value {
        #[prost(bool, tag = "1")]
        DefaultColor(bool),
        #[prost(uint32, tag = "2")]
        PaletteIndex(u32),
        #[prost(uint32, tag = "3")]
        Rgb(u32),
    }
}

macro_rules! terminal_enum {
    ($name:ident { $($variant:ident = $value:expr),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
        #[repr(i32)]
        pub enum $name { $($variant = $value),+ }
    };
}

terminal_enum!(Intensity { Normal = 0, Bold = 1, Faint = 2 });
terminal_enum!(Underline {
    None = 0,
    Single = 1,
    Double = 2,
    Curly = 3,
    Dotted = 4,
    Dashed = 5,
});
terminal_enum!(Blink { None = 0, Slow = 1, Rapid = 2 });
terminal_enum!(SemanticType { Output = 0, Input = 1, Prompt = 2 });
terminal_enum!(VerticalAlign { Baseline = 0, Superscript = 1, Subscript = 2 });

#[derive(Clone, PartialEq, Message)]
pub struct Hyperlink {
    #[prost(uint64, tag = "1")]
    pub id: u64,
    #[prost(string, tag = "2")]
    pub uri: String,
    #[prost(string, tag = "3")]
    pub explicit_id: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct Modes {
    #[prost(bool, tag = "1")]
    pub application_cursor_keys: bool,
    #[prost(bool, tag = "2")]
    pub application_keypad: bool,
    #[prost(bool, tag = "3")]
    pub bracketed_paste: bool,
    #[prost(bool, tag = "4")]
    pub focus_tracking: bool,
    #[prost(bool, tag = "5")]
    pub origin: bool,
    #[prost(bool, tag = "6")]
    pub insert: bool,
    #[prost(bool, tag = "7")]
    pub auto_wrap: bool,
    #[prost(bool, tag = "8")]
    pub reverse_wraparound: bool,
    #[prost(bool, tag = "9")]
    pub newline: bool,
    #[prost(bool, tag = "10")]
    pub left_right_margin: bool,
    #[prost(bool, tag = "11")]
    pub reverse_video: bool,
    #[prost(enumeration = "MouseTracking", tag = "12")]
    pub mouse_tracking: i32,
    #[prost(enumeration = "MouseEncoding", tag = "13")]
    pub mouse_encoding: i32,
    #[prost(enumeration = "KeyboardEncoding", tag = "14")]
    pub keyboard_encoding: i32,
    #[prost(uint32, tag = "15")]
    pub keyboard_flags: u32,
    #[prost(bool, tag = "16")]
    pub alternate_scroll: bool,
}

terminal_enum!(MouseTracking {
    None = 0,
    X10 = 1,
    Vt200 = 2,
    ButtonEvent = 3,
    AnyEvent = 4,
});
terminal_enum!(MouseEncoding { Default = 0, Utf8 = 1, Sgr = 2, SgrPixels = 3 });
terminal_enum!(KeyboardEncoding { Xterm = 0, CsiU = 1, Kitty = 2 });

#[derive(Clone, PartialEq, Message)]
pub struct Palette {
    #[prost(uint32, repeated, packed = "true", tag = "1")]
    pub indexed_rgb: Vec<u32>,
    #[prost(uint32, tag = "2")]
    pub foreground_rgb: u32,
    #[prost(uint32, tag = "3")]
    pub background_rgb: u32,
    #[prost(uint32, tag = "4")]
    pub cursor_fg_rgb: u32,
    #[prost(uint32, tag = "5")]
    pub cursor_bg_rgb: u32,
    #[prost(uint32, tag = "6")]
    pub selection_fg_rgb: u32,
    #[prost(uint32, tag = "7")]
    pub selection_bg_rgb: u32,
}

pub fn validate(state: &State) -> Result<()> {
    ensure!(
        state.schema_version == SCHEMA_VERSION,
        "unsupported terminal state schema version {}",
        state.schema_version
    );
    ensure!(
        state.epoch.len() == EPOCH_BYTES,
        "terminal state epoch must be {EPOCH_BYTES} bytes"
    );
    ensure!(
        state.generation > 0,
        "terminal state generation must be nonzero"
    );
    ensure!(
        (1..=MAX_DIMENSION).contains(&state.rows),
        "terminal rows are out of range"
    );
    ensure!(
        (1..=MAX_DIMENSION).contains(&state.cols),
        "terminal cols are out of range"
    );
    ensure!(
        state.encoded_len() <= MAX_ENCODED_STATE_BYTES,
        "terminal state exceeds {MAX_ENCODED_STATE_BYTES} encoded bytes"
    );
    ensure!(
        state.title.len() <= MAX_TITLE_BYTES,
        "terminal title is too large"
    );
    ensure!(
        state.working_directory.len() <= MAX_WORKING_DIRECTORY_BYTES,
        "terminal working directory is too large"
    );

    let active = ScreenKind::try_from(state.active_screen)
        .map_err(|_| anyhow::anyhow!("unknown active screen enum"))?;
    ensure!(
        active != ScreenKind::Unspecified,
        "active screen is unspecified"
    );

    validate_modes(
        state
            .modes
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("terminal modes are missing"))?,
    )?;
    validate_palette(
        state
            .palette
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("terminal palette is missing"))?,
    )?;

    let (style_ids, hyperlink_ids) = validate_tables(&state.styles, &state.hyperlinks)?;

    let primary = state
        .primary
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("primary screen is missing"))?;
    let alternate = state
        .alternate
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("alternate screen is missing"))?;
    ensure!(
        primary.included_rows.len() + alternate.included_rows.len() <= MAX_INCLUDED_ROWS,
        "too many included terminal rows"
    );

    let mut cell_count = 0usize;
    validate_screen(
        primary,
        false,
        state,
        &style_ids,
        &hyperlink_ids,
        &mut cell_count,
    )?;
    validate_screen(
        alternate,
        true,
        state,
        &style_ids,
        &hyperlink_ids,
        &mut cell_count,
    )?;
    ensure!(cell_count <= MAX_CELLS, "too many terminal cells");
    Ok(())
}

pub fn validate_history_page_request(request: &HistoryPageRequest) -> Result<()> {
    ensure!(
        request.epoch.len() == EPOCH_BYTES,
        "history request epoch must be {EPOCH_BYTES} bytes"
    );
    required_anchor(&request.before, "history request anchor")?;
    ensure!(
        (1..=MAX_HISTORY_PAGE_ROWS as u32).contains(&request.maximum_rows),
        "history request row count is out of range"
    );
    Ok(())
}

pub fn validate_history_page(page: &HistoryPage) -> Result<()> {
    ensure!(page.request_id > 0, "history page request ID is zero");
    ensure!(
        page.epoch.len() == EPOCH_BYTES,
        "history page epoch must be {EPOCH_BYTES} bytes"
    );
    ensure!(page.generation > 0, "history page generation is zero");
    ensure!(
        (1..=MAX_DIMENSION).contains(&page.cols),
        "history page columns are out of range"
    );
    ensure!(
        page.encoded_len() <= MAX_ENCODED_HISTORY_PAGE_BYTES,
        "history page exceeds {MAX_ENCODED_HISTORY_PAGE_BYTES} encoded bytes"
    );
    let oldest = required_anchor(&page.oldest_available, "history oldest available anchor")?;
    let newest = required_anchor(&page.newest_available, "history newest available anchor")?;
    ensure!(
        anchor_key(oldest) <= anchor_key(newest),
        "history available range is reversed"
    );

    if page.reset_required {
        ensure!(
            page.included_rows.is_empty()
                && page.included_start.is_none()
                && page.included_end.is_none()
                && page.styles.is_empty()
                && page.hyperlinks.is_empty()
                && !page.more_before,
            "history reset page contains page data"
        );
        return Ok(());
    }

    ensure!(
        page.included_rows.len() <= MAX_HISTORY_PAGE_ROWS,
        "history page contains too many rows"
    );
    if page.included_rows.is_empty() {
        ensure!(
            page.included_start.is_none()
                && page.included_end.is_none()
                && page.styles.is_empty()
                && page.hyperlinks.is_empty()
                && !page.more_before,
            "empty history page contains range data"
        );
        return Ok(());
    }

    let included_start = required_anchor(&page.included_start, "history included start anchor")?;
    let included_end = required_anchor(&page.included_end, "history included end anchor")?;
    ensure!(
        anchor_key(oldest) <= anchor_key(included_start)
            && anchor_key(included_start) <= anchor_key(included_end)
            && anchor_key(included_end) <= anchor_key(newest),
        "history included range is outside available rows"
    );
    ensure!(
        page.included_rows
            .first()
            .and_then(|row| row.start.as_ref())
            == Some(included_start)
            && page.included_rows.last().and_then(|row| row.start.as_ref()) == Some(included_end),
        "history page boundary anchors do not match rows"
    );
    ensure!(
        page.more_before == (anchor_key(oldest) < anchor_key(included_start)),
        "history page continuation flag is inconsistent"
    );
    let (style_ids, hyperlink_ids) = validate_tables(&page.styles, &page.hyperlinks)?;
    let mut cell_count = 0;
    validate_rows(
        &page.included_rows,
        page.cols,
        page.generation,
        &style_ids,
        &hyperlink_ids,
        &mut cell_count,
    )?;
    Ok(())
}

fn validate_tables(
    styles: &[Style],
    hyperlinks: &[Hyperlink],
) -> Result<(HashSet<u32>, HashSet<u64>)> {
    ensure!(styles.len() <= MAX_STYLES, "too many terminal styles");
    let mut style_ids = HashSet::with_capacity(styles.len());
    for style in styles {
        validate_style(style)?;
        ensure!(
            style.id != 0 && style_ids.insert(style.id),
            "terminal style IDs must be unique and nonzero"
        );
    }

    ensure!(
        hyperlinks.len() <= MAX_HYPERLINKS,
        "too many terminal hyperlinks"
    );
    let mut hyperlink_ids = HashSet::with_capacity(hyperlinks.len());
    for hyperlink in hyperlinks {
        ensure!(
            hyperlink.id != 0 && hyperlink_ids.insert(hyperlink.id),
            "terminal hyperlink IDs must be unique and nonzero"
        );
        ensure!(!hyperlink.uri.is_empty(), "terminal hyperlink URI is empty");
        ensure!(
            hyperlink.uri.len() <= MAX_HYPERLINK_URI_BYTES,
            "terminal hyperlink URI is too large"
        );
        ensure!(
            hyperlink.explicit_id.len() <= MAX_HYPERLINK_EXPLICIT_ID_BYTES,
            "terminal hyperlink explicit ID is too large"
        );
    }
    Ok((style_ids, hyperlink_ids))
}

fn validate_modes(modes: &Modes) -> Result<()> {
    MouseTracking::try_from(modes.mouse_tracking)
        .map_err(|_| anyhow::anyhow!("unknown mouse tracking enum"))?;
    MouseEncoding::try_from(modes.mouse_encoding)
        .map_err(|_| anyhow::anyhow!("unknown mouse encoding enum"))?;
    let keyboard_encoding = KeyboardEncoding::try_from(modes.keyboard_encoding)
        .map_err(|_| anyhow::anyhow!("unknown keyboard encoding enum"))?;
    match keyboard_encoding {
        KeyboardEncoding::Xterm | KeyboardEncoding::CsiU => {
            ensure!(
                modes.keyboard_flags == 0,
                "keyboard flags require kitty encoding"
            );
        }
        KeyboardEncoding::Kitty => {
            ensure!(
                modes.keyboard_flags & !0x1f == 0,
                "unknown kitty keyboard flags"
            );
        }
    }
    Ok(())
}

fn validate_palette(palette: &Palette) -> Result<()> {
    ensure!(
        palette.indexed_rgb.len() == 256,
        "terminal palette must contain 256 indexed colors"
    );
    for rgb in palette.indexed_rgb.iter().copied().chain([
        palette.foreground_rgb,
        palette.background_rgb,
        palette.cursor_fg_rgb,
        palette.cursor_bg_rgb,
        palette.selection_fg_rgb,
        palette.selection_bg_rgb,
    ]) {
        ensure!(rgb <= 0x00ff_ffff, "terminal palette RGB is out of range");
    }
    Ok(())
}

fn validate_style(style: &Style) -> Result<()> {
    validate_color(
        style
            .foreground
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("style foreground is missing"))?,
    )?;
    validate_color(
        style
            .background
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("style background is missing"))?,
    )?;
    validate_color(
        style
            .underline_color
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("style underline color is missing"))?,
    )?;
    Intensity::try_from(style.intensity).map_err(|_| anyhow::anyhow!("unknown intensity enum"))?;
    Underline::try_from(style.underline).map_err(|_| anyhow::anyhow!("unknown underline enum"))?;
    Blink::try_from(style.blink).map_err(|_| anyhow::anyhow!("unknown blink enum"))?;
    SemanticType::try_from(style.semantic_type)
        .map_err(|_| anyhow::anyhow!("unknown semantic type enum"))?;
    VerticalAlign::try_from(style.vertical_align)
        .map_err(|_| anyhow::anyhow!("unknown vertical align enum"))?;
    Ok(())
}

fn validate_color(color: &Color) -> Result<()> {
    match color
        .value
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("style color is missing a value"))?
    {
        color::Value::DefaultColor(_) => Ok(()),
        color::Value::PaletteIndex(index) => {
            ensure!(*index < 256, "style palette index is out of range");
            Ok(())
        }
        color::Value::Rgb(rgb) => {
            ensure!(*rgb <= 0x00ff_ffff, "style RGB is out of range");
            Ok(())
        }
    }
}

fn validate_screen(
    screen: &Screen,
    alternate: bool,
    state: &State,
    style_ids: &HashSet<u32>,
    hyperlink_ids: &HashSet<u64>,
    cell_count: &mut usize,
) -> Result<()> {
    ensure!(
        !screen.included_rows.is_empty(),
        "terminal screen has no included rows"
    );
    let viewport_start = usize::try_from(screen.viewport_start)?;
    if alternate {
        ensure!(
            viewport_start == 0 && screen.included_rows.len() == state.rows as usize,
            "alternate screen must contain exactly one viewport and no history"
        );
    }
    let viewport_end = viewport_start
        .checked_add(state.rows as usize)
        .ok_or_else(|| anyhow::anyhow!("terminal viewport overflows"))?;
    ensure!(
        viewport_end <= screen.included_rows.len(),
        "terminal viewport is not fully included"
    );
    validate_margins_and_tabs(screen, state)?;
    let oldest = required_anchor(&screen.oldest_available, "oldest available anchor")?;
    let newest = required_anchor(&screen.newest_available, "newest available anchor")?;
    let included_start = required_anchor(&screen.included_start, "included start anchor")?;
    let included_end = required_anchor(&screen.included_end, "included end anchor")?;
    ensure!(
        anchor_key(oldest) <= anchor_key(included_start),
        "included rows start before available history"
    );
    ensure!(
        anchor_key(included_start) <= anchor_key(included_end),
        "included row range is reversed"
    );
    ensure!(
        anchor_key(included_end) <= anchor_key(newest),
        "included rows end after available history"
    );

    let first_start = required_anchor(&screen.included_rows[0].start, "first row anchor")?;
    let last_start = required_anchor(
        &screen.included_rows[screen.included_rows.len() - 1].start,
        "last row anchor",
    )?;
    ensure!(
        first_start == included_start && last_start == included_end,
        "included row boundary anchors do not match rows"
    );

    validate_rows(
        &screen.included_rows,
        state.cols,
        state.generation,
        style_ids,
        hyperlink_ids,
        cell_count,
    )?;

    validate_cursor(required_cursor(&screen.cursor, "cursor")?, screen, state)?;
    if let Some(saved) = &screen.saved_cursor {
        validate_cursor(saved, screen, state)?;
    }
    Ok(())
}

fn validate_rows(
    rows: &[Row],
    cols: u32,
    generation: u64,
    style_ids: &HashSet<u32>,
    hyperlink_ids: &HashSet<u64>,
    cell_count: &mut usize,
) -> Result<()> {
    let mut previous: Option<&Anchor> = None;
    for (index, row) in rows.iter().enumerate() {
        let start = required_anchor(&row.start, "row anchor")?;
        ensure!(
            row.row_version > 0 && row.row_version <= generation,
            "row version is outside the state generation"
        );
        if let Some(previous) = previous {
            ensure!(
                anchor_key(previous) < anchor_key(start),
                "terminal row anchors are not strictly ordered"
            );
        }
        if let Some(next) = rows.get(index + 1) {
            let next_start = required_anchor(&next.start, "next row anchor")?;
            if row.wrapped_to_next {
                ensure!(
                    next_start.logical_line_id == start.logical_line_id,
                    "wrapped rows changed logical line ID"
                );
                let expected_offset = start
                    .cell_offset
                    .checked_add(cols)
                    .ok_or_else(|| anyhow::anyhow!("wrapped row offset overflows"))?;
                ensure!(
                    next_start.cell_offset == expected_offset,
                    "wrapped row offset does not advance by terminal cols"
                );
            } else {
                ensure!(
                    next_start.logical_line_id > start.logical_line_id,
                    "hard line break did not advance logical line ID"
                );
            }
        }
        validate_cells(row, cols, style_ids, hyperlink_ids, cell_count)?;
        previous = Some(start);
    }
    Ok(())
}

fn validate_margins_and_tabs(screen: &Screen, state: &State) -> Result<()> {
    ensure!(
        screen.scroll_margin_top < screen.scroll_margin_bottom
            && screen.scroll_margin_bottom <= state.rows,
        "vertical scroll margins are invalid"
    );
    ensure!(
        screen.scroll_margin_left < screen.scroll_margin_right
            && screen.scroll_margin_right <= state.cols,
        "horizontal scroll margins are invalid"
    );
    ensure!(
        screen.tab_stops.len() <= state.cols as usize,
        "too many tab stops"
    );
    let mut previous = None;
    for stop in &screen.tab_stops {
        ensure!(*stop < state.cols, "tab stop is outside terminal cols");
        if let Some(previous) = previous {
            ensure!(previous < *stop, "tab stops are not strictly ordered");
        }
        previous = Some(*stop);
    }
    Ok(())
}

fn validate_cells(
    row: &Row,
    cols: u32,
    style_ids: &HashSet<u32>,
    hyperlink_ids: &HashSet<u64>,
    cell_count: &mut usize,
) -> Result<()> {
    *cell_count = cell_count
        .checked_add(row.cells.len())
        .ok_or_else(|| anyhow::anyhow!("terminal cell count overflows"))?;
    ensure!(*cell_count <= MAX_CELLS, "too many terminal cells");
    let mut previous_end = 0;
    for (index, cell) in row.cells.iter().enumerate() {
        ensure!(!cell.grapheme.is_empty(), "terminal grapheme is empty");
        ensure!(
            cell.grapheme.graphemes(true).count() == 1,
            "terminal cell must contain exactly one grapheme"
        );
        ensure!(
            cell.grapheme.len() <= MAX_GRAPHEME_BYTES,
            "terminal grapheme is too large"
        );
        ensure!(
            matches!(cell.width, 1 | 2),
            "terminal grapheme width must be one or two"
        );
        let end = cell
            .column
            .checked_add(cell.width)
            .ok_or_else(|| anyhow::anyhow!("terminal cell column overflows"))?;
        ensure!(end <= cols, "terminal cell extends beyond terminal cols");
        if index > 0 {
            ensure!(
                cell.column >= previous_end,
                "terminal cells overlap or are out of order"
            );
        }
        ensure!(
            cell.style_id == 0 || style_ids.contains(&cell.style_id),
            "terminal cell references an unknown style"
        );
        ensure!(
            cell.hyperlink_id == 0 || hyperlink_ids.contains(&cell.hyperlink_id),
            "terminal cell references an unknown hyperlink"
        );
        previous_end = end;
    }
    Ok(())
}

fn validate_cursor(cursor: &Cursor, screen: &Screen, state: &State) -> Result<()> {
    ensure!(
        cursor.x < state.cols && cursor.y < state.rows,
        "terminal cursor ({}, {}) is outside {}x{} viewport",
        cursor.x,
        cursor.y,
        state.cols,
        state.rows
    );
    ensure!(
        cursor.version > 0 && cursor.version <= state.generation,
        "terminal cursor version is outside the state generation"
    );
    let shape = CursorShape::try_from(cursor.shape)
        .map_err(|_| anyhow::anyhow!("unknown cursor shape enum"))?;
    ensure!(
        shape != CursorShape::Unspecified,
        "cursor shape is unspecified"
    );
    let viewport_index = screen.viewport_start as usize + cursor.y as usize;
    let row_start = required_anchor(
        &screen.included_rows[viewport_index].start,
        "cursor row anchor",
    )?;
    let anchor = required_anchor(&cursor.anchor, "cursor anchor")?;
    ensure!(
        anchor.logical_line_id == row_start.logical_line_id,
        "cursor anchor has the wrong logical line ID"
    );
    ensure!(
        anchor.cell_offset == row_start.cell_offset + cursor.x,
        "cursor anchor has the wrong cell offset"
    );
    Ok(())
}

fn required_anchor<'a>(anchor: &'a Option<Anchor>, name: &str) -> Result<&'a Anchor> {
    let anchor = anchor
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("{name} is missing"))?;
    ensure!(
        anchor.logical_line_id != 0,
        "{name} has logical line ID zero"
    );
    Ok(anchor)
}

fn required_cursor<'a>(cursor: &'a Option<Cursor>, name: &str) -> Result<&'a Cursor> {
    cursor
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("{name} is missing"))
}

fn anchor_key(anchor: &Anchor) -> (u64, u32) {
    (anchor.logical_line_id, anchor.cell_offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    const VALID_STATE_GOLDEN_SHA256: &str =
        "a0d4babf5836c43c875c7fa3bbd0c856ba46dbccd7c5ec1ac98fba7c1206bb1a";

    fn anchor(logical_line_id: u64, cell_offset: u32) -> Anchor {
        Anchor {
            logical_line_id,
            cell_offset,
        }
    }

    fn screen(logical_line_id: u64, rows: u32, cols: u32) -> Screen {
        let included_rows: Vec<_> = (0..rows)
            .map(|row| Row {
                start: Some(anchor(logical_line_id + row as u64, 0)),
                row_version: 7,
                cells: if row == 0 {
                    vec![Cell {
                        column: 0,
                        grapheme: "界".into(),
                        width: 2,
                        style_id: 1,
                        hyperlink_id: 1,
                    }]
                } else {
                    vec![]
                },
                wrapped_to_next: false,
            })
            .collect();
        Screen {
            included_start: included_rows.first().unwrap().start.clone(),
            included_end: included_rows.last().unwrap().start.clone(),
            oldest_available: included_rows.first().unwrap().start.clone(),
            newest_available: included_rows.last().unwrap().start.clone(),
            included_rows,
            viewport_start: 0,
            cursor: Some(Cursor {
                x: 0,
                y: 0,
                anchor: Some(anchor(logical_line_id, 0)),
                shape: CursorShape::Block as i32,
                visible: true,
                version: 7,
                wrap_pending: false,
            }),
            saved_cursor: None,
            scroll_margin_top: 0,
            scroll_margin_bottom: rows,
            scroll_margin_left: 0,
            scroll_margin_right: cols,
            tab_stops: (8..cols).step_by(8).collect(),
        }
    }

    fn valid_state() -> State {
        State {
            schema_version: SCHEMA_VERSION,
            epoch: vec![7; EPOCH_BYTES],
            generation: 7,
            rows: 2,
            cols: 10,
            primary: Some(screen(1, 2, 10)),
            alternate: Some(screen(100, 2, 10)),
            active_screen: ScreenKind::Primary as i32,
            styles: vec![Style {
                id: 1,
                foreground: Some(Color {
                    value: Some(color::Value::PaletteIndex(2)),
                }),
                background: Some(Color {
                    value: Some(color::Value::DefaultColor(true)),
                }),
                underline_color: Some(Color {
                    value: Some(color::Value::Rgb(0x12_34_56)),
                }),
                intensity: Intensity::Bold as i32,
                underline: Underline::Single as i32,
                blink: Blink::None as i32,
                italic: true,
                reverse: false,
                strikethrough: false,
                invisible: false,
                overline: false,
                semantic_type: SemanticType::Output as i32,
                vertical_align: VerticalAlign::Baseline as i32,
            }],
            hyperlinks: vec![Hyperlink {
                id: 1,
                uri: "https://example.test".into(),
                explicit_id: "link".into(),
            }],
            modes: Some(Modes {
                application_cursor_keys: true,
                application_keypad: false,
                bracketed_paste: true,
                focus_tracking: false,
                origin: false,
                insert: false,
                auto_wrap: true,
                reverse_wraparound: false,
                newline: false,
                left_right_margin: false,
                reverse_video: false,
                mouse_tracking: MouseTracking::None as i32,
                mouse_encoding: MouseEncoding::Default as i32,
                keyboard_encoding: KeyboardEncoding::Xterm as i32,
                keyboard_flags: 0,
                alternate_scroll: true,
            }),
            title: "Astra".into(),
            working_directory: "/tmp".into(),
            palette: Some(Palette {
                indexed_rgb: vec![0; 256],
                foreground_rgb: 0xff_ff_ff,
                background_rgb: 0,
                cursor_fg_rgb: 0,
                cursor_bg_rgb: 0xff_ff_ff,
                selection_fg_rgb: 0xff_ff_ff,
                selection_bg_rgb: 0x33_33_33,
            }),
        }
    }

    #[test]
    fn valid_state_round_trips_and_validates() {
        let state = valid_state();
        validate(&state).unwrap();
        let bytes = state.encode_to_vec();
        assert_eq!(
            format!("{:x}", Sha256::digest(&bytes)),
            VALID_STATE_GOLDEN_SHA256
        );
        let decoded = State::decode(bytes.as_slice()).unwrap();
        assert_eq!(decoded, state);
        validate(&decoded).unwrap();
    }

    #[test]
    fn rejects_physical_row_identity_after_reflow() {
        let mut state = valid_state();
        {
            let primary = state.primary.as_mut().unwrap();
            primary.included_rows[0].wrapped_to_next = true;
            primary.included_rows[1].start = Some(anchor(1, state.cols));
            primary.included_end = primary.included_rows[1].start.clone();
            primary.newest_available = primary.included_end.clone();
        }
        validate(&state).unwrap();

        {
            let primary = state.primary.as_mut().unwrap();
            primary.included_rows[1].start = Some(anchor(2, 0));
            primary.included_end = primary.included_rows[1].start.clone();
            primary.newest_available = primary.included_end.clone();
        }
        assert!(
            validate(&state)
                .unwrap_err()
                .to_string()
                .contains("wrapped rows changed logical line ID")
        );
    }

    #[test]
    fn rejects_unknown_references_and_partial_alternate_screen() {
        let mut state = valid_state();
        state.primary.as_mut().unwrap().included_rows[0].cells[0].style_id = 99;
        assert!(
            validate(&state)
                .unwrap_err()
                .to_string()
                .contains("unknown style")
        );

        let mut state = valid_state();
        state.alternate.as_mut().unwrap().included_rows.pop();
        assert!(
            validate(&state)
                .unwrap_err()
                .to_string()
                .contains("alternate screen")
        );
    }

    #[test]
    fn rejects_multiple_graphemes_in_one_cell() {
        let mut state = valid_state();
        state.primary.as_mut().unwrap().included_rows[0].cells[0].grapheme = "ab".into();
        assert!(validate(&state).is_err());
    }

    #[test]
    fn rejects_invalid_epoch_generation_and_dimensions() {
        let mut state = valid_state();
        state.epoch.pop();
        assert!(validate(&state).unwrap_err().to_string().contains("epoch"));

        let mut state = valid_state();
        state.generation = 0;
        assert!(
            validate(&state)
                .unwrap_err()
                .to_string()
                .contains("generation")
        );

        let mut state = valid_state();
        state.cols = MAX_DIMENSION + 1;
        assert!(validate(&state).unwrap_err().to_string().contains("cols"));
    }

    #[test]
    fn rejects_oversized_and_unknown_enum_state() {
        let mut state = valid_state();
        state.title = "x".repeat(MAX_TITLE_BYTES + 1);
        assert!(validate(&state).unwrap_err().to_string().contains("title"));

        let mut state = valid_state();
        state.modes.as_mut().unwrap().mouse_tracking = 99;
        assert!(
            validate(&state)
                .unwrap_err()
                .to_string()
                .contains("mouse tracking")
        );

        let mut state = valid_state();
        state.modes.as_mut().unwrap().keyboard_flags = 1;
        assert!(
            validate(&state)
                .unwrap_err()
                .to_string()
                .contains("kitty encoding")
        );

        let mut state = valid_state();
        let modes = state.modes.as_mut().unwrap();
        modes.keyboard_encoding = KeyboardEncoding::Kitty as i32;
        modes.keyboard_flags = 0x20;
        assert!(
            validate(&state)
                .unwrap_err()
                .to_string()
                .contains("kitty keyboard flags")
        );
    }

    #[test]
    fn validates_bounded_history_requests_pages_and_resets() {
        let state = valid_state();
        let primary = state.primary.unwrap();
        let request = HistoryPageRequest {
            epoch: state.epoch.clone(),
            before: primary.included_end.clone(),
            maximum_rows: 2,
        };
        validate_history_page_request(&request).unwrap();

        let mut page = HistoryPage {
            request_id: 3,
            epoch: state.epoch,
            generation: state.generation,
            cols: state.cols,
            included_rows: primary.included_rows,
            oldest_available: primary.oldest_available,
            newest_available: primary.newest_available,
            included_start: primary.included_start,
            included_end: primary.included_end,
            styles: state.styles,
            hyperlinks: state.hyperlinks,
            more_before: false,
            reset_required: false,
        };
        validate_history_page(&page).unwrap();

        page.included_rows[1].start = Some(anchor(1, state.cols));
        assert!(validate_history_page(&page).is_err());

        let reset = HistoryPage {
            request_id: 4,
            epoch: vec![8; EPOCH_BYTES],
            generation: 1,
            cols: state.cols,
            included_rows: vec![],
            oldest_available: Some(anchor(10, 0)),
            newest_available: Some(anchor(11, 0)),
            included_start: None,
            included_end: None,
            styles: vec![],
            hyperlinks: vec![],
            more_before: false,
            reset_required: true,
        };
        validate_history_page(&reset).unwrap();
    }
}
