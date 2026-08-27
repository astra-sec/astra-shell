use super::*;
use crate::{AstraLineIdentity, Line};
use wezterm_cell::CellAttributes;
use wezterm_surface::line::CellRef;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AstraScreenKind {
    Primary,
    Alternate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AstraCursorShape {
    Block,
    Underline,
    Bar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AstraMouseTracking {
    None,
    X10,
    Vt200,
    ButtonEvent,
    AnyEvent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AstraMouseEncoding {
    Default,
    Utf8,
    Sgr,
    SgrPixels,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AstraKeyboardEncoding {
    Xterm,
    CsiU,
    Kitty { flags: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AstraCursorView {
    pub x: usize,
    pub y: usize,
    pub shape: AstraCursorShape,
    pub visible: bool,
    pub version: u64,
    pub wrap_pending: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AstraModesView {
    pub application_cursor_keys: bool,
    pub application_keypad: bool,
    pub bracketed_paste: bool,
    pub focus_tracking: bool,
    pub origin: bool,
    pub insert: bool,
    pub auto_wrap: bool,
    pub reverse_wraparound: bool,
    pub newline: bool,
    pub left_right_margin: bool,
    pub reverse_video: bool,
    pub alternate_scroll: bool,
    pub mouse_tracking: AstraMouseTracking,
    pub mouse_encoding: AstraMouseEncoding,
    pub keyboard_encoding: AstraKeyboardEncoding,
}

pub struct AstraTerminalView<'a> {
    pub primary: AstraScreenView<'a>,
    pub alternate: AstraScreenView<'a>,
    pub active_screen: AstraScreenKind,
    pub modes: AstraModesView,
    pub title: &'a str,
    pub title_was_set: bool,
    pub working_directory: Option<&'a str>,
    pub palette: ColorPalette,
    pub sequence: u64,
    /// Changes when either screen has to invalidate old logical anchors.
    pub identity_epoch: (u64, u64),
}

pub struct AstraScreenView<'a> {
    screen: &'a Screen,
    kind: AstraScreenKind,
    pub cursor: AstraCursorView,
    pub saved_cursor: Option<AstraCursorView>,
    pub scroll_margin_top: usize,
    pub scroll_margin_bottom: usize,
    pub scroll_margin_left: usize,
    pub scroll_margin_right: usize,
    tabs: &'a TabStop,
}

pub struct AstraRowView<'a> {
    pub identity: AstraLineIdentity,
    pub version: u64,
    line: &'a Line,
}

pub struct AstraCellView<'a> {
    cell: CellRef<'a>,
}

impl AstraTerminalView<'_> {
    pub fn rows(&self) -> usize {
        self.primary.screen.physical_rows
    }

    pub fn columns(&self) -> usize {
        self.primary.screen.physical_cols
    }
}

impl<'a> AstraScreenView<'a> {
    pub fn kind(&self) -> AstraScreenKind {
        self.kind
    }

    pub fn row_count(&self) -> usize {
        self.screen.astra_row_count()
    }

    pub fn viewport_start(&self) -> usize {
        self.screen.astra_viewport_start()
    }

    pub fn history_row_count(&self) -> usize {
        self.screen.astra_history_row_count()
    }

    pub fn allows_scrollback(&self) -> bool {
        self.screen.astra_allows_scrollback()
    }

    pub fn rows(
        &self,
        start: usize,
        maximum_rows: usize,
    ) -> impl Iterator<Item = AstraRowView<'a>> {
        self.screen
            .astra_rows(start, maximum_rows)
            .map(|(identity, line)| AstraRowView {
                identity,
                version: line.current_seqno() as u64,
                line,
            })
    }

    pub fn tab_stops(&self) -> impl Iterator<Item = usize> + '_ {
        self.tabs
            .tabs
            .iter()
            .enumerate()
            .filter_map(|(column, stopped)| stopped.then_some(column))
    }
}

impl<'a> AstraRowView<'a> {
    pub fn cells(&self) -> impl Iterator<Item = AstraCellView<'a>> {
        self.line.visible_cells().map(|cell| AstraCellView { cell })
    }

    pub fn wrapped_to_next(&self) -> bool {
        self.line.last_cell_was_wrapped()
    }
}

impl AstraCellView<'_> {
    pub fn column(&self) -> usize {
        self.cell.cell_index()
    }

    pub fn grapheme(&self) -> &str {
        self.cell.str()
    }

    pub fn width(&self) -> usize {
        self.cell.width()
    }

    pub fn attributes(&self) -> &CellAttributes {
        self.cell.attrs()
    }
}

impl TerminalState {
    pub(crate) fn astra_finish_update(&mut self) {
        self.screen.screen.astra_finish_update();
        self.screen.alt_screen.astra_finish_update();
    }

    pub fn astra_view(&self) -> AstraTerminalView<'_> {
        let active_screen = if self.screen.alt_screen_is_active {
            AstraScreenKind::Alternate
        } else {
            AstraScreenKind::Primary
        };
        let current_position = self.cursor_pos();
        let current_cursor = cursor_view(
            current_position,
            self.wrap_next || current_position.x >= self.screen().physical_cols,
        );
        let primary_cursor = if active_screen == AstraScreenKind::Primary {
            current_cursor
        } else {
            self.screen
                .screen
                .saved_cursor
                .as_ref()
                .map(|saved| {
                    cursor_view(
                        saved.position,
                        saved.wrap_next || saved.position.x >= self.screen.screen.physical_cols,
                    )
                })
                .unwrap_or_else(|| default_cursor(&self.screen.screen))
        };
        let alternate_cursor = if active_screen == AstraScreenKind::Alternate {
            current_cursor
        } else {
            self.screen
                .alt_screen
                .saved_cursor
                .as_ref()
                .map(|saved| {
                    cursor_view(
                        saved.position,
                        saved.wrap_next || saved.position.x >= self.screen.alt_screen.physical_cols,
                    )
                })
                .unwrap_or_else(|| default_cursor(&self.screen.alt_screen))
        };
        AstraTerminalView {
            primary: screen_view(
                self,
                &self.screen.screen,
                AstraScreenKind::Primary,
                primary_cursor,
            ),
            alternate: screen_view(
                self,
                &self.screen.alt_screen,
                AstraScreenKind::Alternate,
                alternate_cursor,
            ),
            active_screen,
            modes: AstraModesView {
                application_cursor_keys: self.application_cursor_keys,
                application_keypad: self.application_keypad,
                bracketed_paste: self.bracketed_paste,
                focus_tracking: self.focus_tracking,
                origin: self.dec_origin_mode,
                insert: self.insert,
                auto_wrap: self.dec_auto_wrap,
                reverse_wraparound: self.reverse_wraparound_mode,
                newline: self.newline_mode,
                left_right_margin: self.left_and_right_margin_mode,
                reverse_video: self.reverse_video_mode,
                alternate_scroll: self.alternate_scroll,
                mouse_tracking: if self.any_event_mouse {
                    AstraMouseTracking::AnyEvent
                } else if self.button_event_mouse {
                    AstraMouseTracking::ButtonEvent
                } else if self.mouse_tracking {
                    AstraMouseTracking::Vt200
                } else if self.x10_mouse {
                    AstraMouseTracking::X10
                } else {
                    AstraMouseTracking::None
                },
                mouse_encoding: match self.mouse_encoding {
                    MouseEncoding::X10 => AstraMouseEncoding::Default,
                    MouseEncoding::Utf8 => AstraMouseEncoding::Utf8,
                    MouseEncoding::SGR => AstraMouseEncoding::Sgr,
                    MouseEncoding::SgrPixels => AstraMouseEncoding::SgrPixels,
                },
                keyboard_encoding: match self.get_keyboard_encoding() {
                    KeyboardEncoding::Xterm | KeyboardEncoding::Win32 => {
                        AstraKeyboardEncoding::Xterm
                    }
                    KeyboardEncoding::CsiU => AstraKeyboardEncoding::CsiU,
                    KeyboardEncoding::Kitty(flags) => AstraKeyboardEncoding::Kitty {
                        flags: u32::from(flags.bits()),
                    },
                },
            },
            title: self.get_title(),
            title_was_set: self.astra_title_was_set,
            working_directory: self.get_current_dir().map(Url::as_str),
            palette: self.palette(),
            sequence: self.current_seqno() as u64,
            identity_epoch: (
                self.screen.screen.astra_identity_epoch(),
                self.screen.alt_screen.astra_identity_epoch(),
            ),
        }
    }
}

fn screen_view<'a>(
    state: &'a TerminalState,
    screen: &'a Screen,
    kind: AstraScreenKind,
    cursor: AstraCursorView,
) -> AstraScreenView<'a> {
    AstraScreenView {
        screen,
        kind,
        cursor,
        saved_cursor: screen
            .saved_cursor
            .as_ref()
            .map(|saved| {
                cursor_view(
                    saved.position,
                    saved.wrap_next || saved.position.x >= screen.physical_cols,
                )
            }),
        scroll_margin_top: state.top_and_bottom_margins.start.max(0) as usize,
        scroll_margin_bottom: state.top_and_bottom_margins.end.max(0) as usize,
        scroll_margin_left: state.left_and_right_margins.start,
        scroll_margin_right: state.left_and_right_margins.end,
        tabs: &state.tabs,
    }
}

fn cursor_view(cursor: CursorPosition, wrap_pending: bool) -> AstraCursorView {
    AstraCursorView {
        x: cursor.x,
        y: cursor.y.max(0) as usize,
        shape: match cursor.shape {
            CursorShape::Default | CursorShape::BlinkingBlock | CursorShape::SteadyBlock => {
                AstraCursorShape::Block
            }
            CursorShape::BlinkingUnderline | CursorShape::SteadyUnderline => {
                AstraCursorShape::Underline
            }
            CursorShape::BlinkingBar | CursorShape::SteadyBar => AstraCursorShape::Bar,
        },
        visible: cursor.visibility == CursorVisibility::Visible,
        version: cursor.seqno as u64,
        wrap_pending,
    }
}

fn default_cursor(screen: &Screen) -> AstraCursorView {
    AstraCursorView {
        x: 0,
        y: 0,
        shape: AstraCursorShape::Block,
        visible: true,
        version: screen
            .astra_rows(screen.astra_viewport_start(), 1)
            .next()
            .map(|(_, line)| line.current_seqno() as u64)
            .unwrap_or(1),
        wrap_pending: false,
    }
}
