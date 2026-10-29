use std::collections::HashMap;

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
};
use serde::Deserialize;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::kat_adapter;

pub const DEFAULT_TAB_WIDTH: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffStatus {
    Modified,
    Created,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Unmerged,
}

impl DiffStatus {
    pub fn short_code(self) -> &'static str {
        match self {
            Self::Modified => "M",
            Self::Created => "A",
            Self::Deleted => "D",
            Self::Renamed => "R",
            Self::Copied => "C",
            Self::TypeChanged => "T",
            Self::Unmerged => "U",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffHighlightKind {
    Delimiter,
    Normal,
    String,
    Type,
    Comment,
    Keyword,
    TreeSitterError,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffRange {
    pub start: usize,
    pub end: usize,
    pub kind: DiffHighlightKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffSide {
    pub line_number: usize,
    pub text: String,
    pub diff_ranges: Vec<DiffRange>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffRow {
    pub omitted: bool,
    pub lhs: Option<DiffSide>,
    pub rhs: Option<DiffSide>,
}

impl DiffRow {
    pub fn new(lhs: Option<DiffSide>, rhs: Option<DiffSide>) -> Self {
        Self {
            omitted: false,
            lhs,
            rhs,
        }
    }

    pub fn omitted() -> Self {
        Self {
            omitted: true,
            lhs: None,
            rhs: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffHunk {
    pub title: Option<String>,
    pub rows: Vec<DiffRow>,
}

#[derive(Debug, Clone)]
pub struct FileDiff {
    pub path: String,
    pub old_path: Option<String>,
    pub status: DiffStatus,
    pub line_diff_reason: Option<String>,
    pub metadata_lines: Vec<String>,
    pub hunks: Vec<DiffHunk>,
    pub lhs_syntax: Vec<Vec<kat_adapter::SyntaxRange>>,
    pub rhs_syntax: Vec<Vec<kat_adapter::SyntaxRange>>,
    pub lhs_document: Option<kat::PreparedDocument>,
    pub rhs_document: Option<kat::PreparedDocument>,
}

impl FileDiff {
    pub fn is_empty(&self) -> bool {
        self.line_diff_reason.is_none() && self.metadata_lines.is_empty() && self.hunks.is_empty()
    }

    pub fn display_name(&self) -> String {
        match self.status {
            DiffStatus::Renamed | DiffStatus::Copied => format!(
                "[{}] {} -> {}",
                self.status.short_code(),
                self.old_path.as_deref().unwrap_or("?"),
                self.path
            ),
            _ => format!("[{}] {}", self.status.short_code(), self.path),
        }
    }

    fn syntax_for_side(
        &self,
        side_kind: SideKind,
        line_number: usize,
    ) -> &[kat_adapter::SyntaxRange] {
        let index = line_number.saturating_sub(1);
        match side_kind {
            SideKind::Left => self.lhs_syntax.get(index).map(Vec::as_slice).unwrap_or(&[]),
            SideKind::Right => self.rhs_syntax.get(index).map(Vec::as_slice).unwrap_or(&[]),
        }
    }
}

struct FileBackgrounds {
    lhs: Vec<Vec<kat::HighlightBackgroundRun>>,
    rhs: Vec<Vec<kat::HighlightBackgroundRun>>,
}

impl FileBackgrounds {
    fn from_file(file: &FileDiff, tab_width: usize) -> Self {
        let background_lines = |document: &Option<kat::PreparedDocument>| {
            document
                .as_ref()
                .map(|document| document.background_lines(tab_width))
                .unwrap_or_default()
        };
        Self {
            lhs: background_lines(&file.lhs_document),
            rhs: background_lines(&file.rhs_document),
        }
    }

    fn line(&self, side_kind: SideKind, line_number: usize) -> &[kat::HighlightBackgroundRun] {
        let lines = match side_kind {
            SideKind::Left => &self.lhs,
            SideKind::Right => &self.rhs,
        };
        lines
            .get(line_number.saturating_sub(1))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLayout {
    content_width: usize,
    tab_width: usize,
    visual_line_count: usize,
    visual_hunk_starts: Vec<usize>,
    rendered_text: Text<'static>,
}

impl DiffLayout {
    #[cfg(test)]
    pub fn build(file: &FileDiff, content_width: usize) -> Self {
        Self::with_tab_width(file, content_width, DEFAULT_TAB_WIDTH)
    }

    pub fn with_tab_width(file: &FileDiff, content_width: usize, tab_width: usize) -> Self {
        let content_width = content_width.max(1);
        let tab_width = tab_width.max(1);
        let rendered = render_file(file, content_width, tab_width);
        Self {
            content_width,
            tab_width,
            visual_line_count: rendered.lines.len(),
            visual_hunk_starts: rendered.hunk_starts,
            rendered_text: Text::from(rendered.lines),
        }
    }

    pub fn content_width(&self) -> usize {
        self.content_width
    }

    pub fn tab_width(&self) -> usize {
        self.tab_width
    }

    pub fn visual_line_count(&self) -> usize {
        self.visual_line_count
    }

    pub fn visual_hunk_starts(&self) -> &[usize] {
        &self.visual_hunk_starts
    }

    pub fn rendered_text(&self) -> &Text<'static> {
        &self.rendered_text
    }
}

#[derive(Debug, Clone)]
struct ActiveDiffLayout {
    file_path: String,
    layout: DiffLayout,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UiLayout {
    pub sidebar_inner: Rect,
    pub diff_area: Rect,
    pub diff_inner: Rect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    FileList,
    Diff,
}

const COMPACT_LAYOUT_WIDTH: u16 = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarEntry {
    pub file_index: usize,
    pub status: DiffStatus,
    pub display_path: String,
    pub original_path: String,
    pub matched_positions: Vec<usize>,
    pub is_selected: bool,
    pub is_active_file: bool,
}

impl SidebarEntry {
    pub fn label(&self) -> String {
        format!("[{}] {}", self.status.short_code(), self.display_path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SidebarSearchState {
    query: String,
    matches: Vec<usize>,
    cursor: usize,
    origin_file: usize,
}

#[derive(Debug)]
pub struct AppState {
    pub files: Vec<FileDiff>,
    pub committed_file: usize,
    pub preview_file: Option<usize>,
    pub scroll: usize,
    pub diff_viewport_height: usize,
    pub diff_content_width: usize,
    pub terminal_width: u16,
    pub route: Route,
    pub show_sidebar: bool,
    pub sidebar_scroll: usize,
    pub sidebar_viewport_height: usize,
    pub sidebar_width: u16,
    pub tab_width: usize,
    active_diff_layout: Option<ActiveDiffLayout>,
    ui_layout: UiLayout,
    sidebar_labels: Vec<String>,
    sidebar_search: Option<SidebarSearchState>,
}

impl AppState {
    #[cfg(test)]
    pub fn new(files: Vec<FileDiff>, terminal_width: u16) -> Self {
        Self::with_tab_width(files, terminal_width, DEFAULT_TAB_WIDTH)
    }

    pub fn with_tab_width(files: Vec<FileDiff>, terminal_width: u16, tab_width: usize) -> Self {
        let mut app = Self {
            files,
            committed_file: 0,
            preview_file: None,
            scroll: 0,
            diff_viewport_height: 1,
            diff_content_width: 1,
            terminal_width,
            route: Route::FileList,
            show_sidebar: true,
            sidebar_scroll: 0,
            sidebar_viewport_height: 1,
            sidebar_width: 16,
            tab_width: tab_width.max(1),
            active_diff_layout: None,
            ui_layout: UiLayout::default(),
            sidebar_labels: Vec::new(),
            sidebar_search: None,
        };
        app.rebuild_sidebar_labels();
        app
    }

    pub fn current_file(&self) -> Option<&FileDiff> {
        self.files.get(self.active_file_index())
    }

    pub fn current_rendered_text(&self) -> Option<&Text<'static>> {
        self.current_diff_layout().map(DiffLayout::rendered_text)
    }

    pub fn visible_sidebar_entries(&self) -> Vec<SidebarEntry> {
        self.visible_file_indices()
            .into_iter()
            .map(|(file_index, file)| SidebarEntry {
                file_index,
                status: file.status,
                display_path: self.display_path_for_sidebar_entry(file_index, file),
                original_path: file.path.clone(),
                matched_positions: self.sidebar_entry_match_positions(file_index, file),
                is_selected: Some(file_index) == self.current_sidebar_file_index(),
                is_active_file: file_index == self.active_file_index(),
            })
            .collect()
    }

    pub fn is_sidebar_search_active(&self) -> bool {
        self.sidebar_search.is_some()
    }

    pub fn sidebar_search_query(&self) -> Option<&str> {
        self.sidebar_search
            .as_ref()
            .map(|state| state.query.as_str())
    }

    #[cfg(test)]
    pub fn replace_files(&mut self, files: Vec<FileDiff>) {
        let current_path = self.current_file().map(|f| f.path.clone());
        self.files = files;
        self.scroll = 0;
        self.active_diff_layout = None;
        self.rebuild_sidebar_labels();

        if self.files.is_empty() {
            self.committed_file = 0;
            self.preview_file = None;
            return;
        }

        if let Some(path) = current_path.as_deref()
            && let Some(idx) = self
                .files
                .iter()
                .position(|f| f.path == path || f.old_path.as_deref() == Some(path))
        {
            self.commit_file_by_index(idx);
        }
    }

    pub fn update_ui_layout(&mut self, layout: UiLayout) {
        self.ui_layout = layout;
        self.sidebar_viewport_height = layout.sidebar_inner.height.max(1) as usize;
        self.diff_viewport_height = layout.diff_inner.height.max(1) as usize;
        let next_width = layout.diff_inner.width.max(1) as usize;
        if self.diff_content_width != next_width {
            self.diff_content_width = next_width;
            self.active_diff_layout = None;
        }
        self.ensure_current_diff_layout();
        self.clamp_scroll();
        self.ensure_sidebar_selection_visible();
    }

    pub fn sync_terminal_width(&mut self, terminal_width: u16) {
        self.terminal_width = terminal_width;
        self.sidebar_width = compute_sidebar_width(terminal_width, &self.visible_sidebar_entries());
    }

    pub fn is_compact_layout(&self) -> bool {
        self.terminal_width < COMPACT_LAYOUT_WIDTH
    }

    pub fn is_sidebar_visible(&self) -> bool {
        !self.is_compact_layout() && self.show_sidebar
    }

    pub fn cycle_focus(&mut self) {
        if self.is_sidebar_search_active() {
            return;
        }
        if self.is_compact_layout() {
            self.route = match self.route {
                Route::FileList => Route::Diff,
                Route::Diff => Route::FileList,
            };
            return;
        }
        if self.show_sidebar {
            self.route = match self.route {
                Route::FileList => Route::Diff,
                Route::Diff => Route::FileList,
            };
        } else {
            self.route = Route::Diff;
        }
    }

    pub fn toggle_sidebar(&mut self) {
        if self.is_compact_layout() {
            return;
        }
        self.sidebar_search = None;
        self.show_sidebar = !self.show_sidebar;
        self.route = if self.show_sidebar {
            Route::FileList
        } else {
            Route::Diff
        };
        self.ensure_sidebar_selection_visible();
    }

    pub fn move_cursor_up(&mut self) {
        match self.route {
            Route::FileList => self.move_sidebar_selection(-1),
            Route::Diff => self.scroll_lines(-1),
        }
    }

    pub fn move_cursor_down(&mut self) {
        match self.route {
            Route::FileList => self.move_sidebar_selection(1),
            Route::Diff => self.scroll_lines(1),
        }
    }

    pub fn move_cursor_left(&mut self) {
        if self.is_compact_layout() || self.show_sidebar {
            self.route = Route::FileList;
        }
    }

    pub fn move_cursor_right(&mut self) {
        self.route = Route::Diff;
    }

    pub fn page_down(&mut self) {
        match self.route {
            Route::FileList => {
                let delta = self.sidebar_viewport_height.saturating_sub(1).max(1) as isize;
                self.move_sidebar_selection(delta);
            }
            Route::Diff => self.scroll_lines(self.diff_viewport_height.saturating_sub(1) as isize),
        }
    }

    pub fn page_up(&mut self) {
        match self.route {
            Route::FileList => {
                let delta = self.sidebar_viewport_height.saturating_sub(1).max(1) as isize;
                self.move_sidebar_selection(-delta);
            }
            Route::Diff => {
                self.scroll_lines(-(self.diff_viewport_height.saturating_sub(1) as isize))
            }
        }
    }

    pub fn half_page_down(&mut self) {
        self.scroll_half_page(1);
    }

    pub fn half_page_up(&mut self) {
        self.scroll_half_page(-1);
    }

    fn scroll_half_page(&mut self, direction: isize) {
        match self.route {
            Route::FileList => self.move_sidebar_selection(
                (self.sidebar_viewport_height / 2).max(1) as isize * direction,
            ),
            Route::Diff => {
                self.scroll_lines((self.diff_viewport_height / 2).max(1) as isize * direction)
            }
        }
    }

    pub fn jump_top(&mut self) {
        match self.route {
            Route::FileList => {
                if let Some(state) = self.sidebar_search.as_mut() {
                    state.cursor = 0;
                    self.preview_sidebar_search_selection();
                    self.ensure_sidebar_selection_visible();
                } else {
                    self.select_sidebar_entry_at(0);
                }
            }
            Route::Diff => self.scroll = 0,
        }
    }

    pub fn jump_bottom(&mut self) {
        match self.route {
            Route::FileList => {
                if let Some(state) = self.sidebar_search.as_mut() {
                    if !state.matches.is_empty() {
                        state.cursor = state.matches.len() - 1;
                        self.preview_sidebar_search_selection();
                        self.ensure_sidebar_selection_visible();
                    }
                } else {
                    let entries = self.visible_sidebar_entries();
                    if !entries.is_empty() {
                        self.select_sidebar_entry_at(entries.len() - 1);
                    }
                }
            }
            Route::Diff => {
                self.ensure_current_diff_layout();
                self.scroll = self.max_scroll();
            }
        }
    }

    pub fn jump_next_hunk(&mut self) {
        self.ensure_current_diff_layout();
        let Some(layout) = self.current_diff_layout() else {
            return;
        };

        for &idx in layout.visual_hunk_starts() {
            if idx > self.scroll {
                self.scroll = idx.min(self.max_anchor_scroll());
                return;
            }
        }
    }

    pub fn jump_prev_hunk(&mut self) {
        self.ensure_current_diff_layout();
        let Some(layout) = self.current_diff_layout() else {
            return;
        };

        for &idx in layout.visual_hunk_starts().iter().rev() {
            if idx < self.scroll {
                self.scroll = idx.min(self.max_anchor_scroll());
                return;
            }
        }
    }

    pub fn select_current(&mut self) {
        if self.route != Route::FileList {
            return;
        }

        if let Some(index) = self.current_sidebar_entry_index() {
            self.select_sidebar_entry_at(index);
        }
        self.sidebar_search = None;
        self.route = Route::Diff;
    }

    pub fn escape(&mut self) {
        if let Some(state) = self.sidebar_search.take() {
            self.set_active_file(Some(state.origin_file), true);
            self.route = Route::FileList;
            self.ensure_sidebar_selection_visible();
            return;
        }
        if self.is_compact_layout() && self.route == Route::Diff {
            self.route = Route::FileList;
            return;
        }
        self.route = if self.show_sidebar {
            Route::FileList
        } else {
            Route::Diff
        };
    }

    pub fn should_quit_on_q(&mut self) -> bool {
        if self.is_compact_layout() && self.route == Route::Diff {
            self.route = Route::FileList;
            return false;
        }
        true
    }

    pub fn start_sidebar_search(&mut self) {
        if self.files.is_empty() {
            return;
        }

        self.show_sidebar = true;
        self.route = Route::FileList;
        let selected_match = self
            .files
            .iter()
            .enumerate()
            .position(|(index, _)| index == self.committed_file)
            .unwrap_or(0);
        self.sidebar_search = Some(SidebarSearchState {
            query: String::new(),
            matches: (0..self.files.len()).collect(),
            cursor: selected_match.min(self.files.len().saturating_sub(1)),
            origin_file: self.committed_file,
        });
        self.preview_sidebar_search_selection();
        self.ensure_sidebar_selection_visible();
    }

    pub fn sidebar_search_push(&mut self, ch: char) {
        let Some(state) = self.sidebar_search.as_mut() else {
            return;
        };
        state.query.push(ch);
        self.refresh_sidebar_search_matches();
    }

    pub fn sidebar_search_backspace(&mut self) {
        let Some(state) = self.sidebar_search.as_mut() else {
            return;
        };
        state.query.pop();
        self.refresh_sidebar_search_matches();
    }

    pub fn toggle_current_fold(&mut self) {
        self.ensure_sidebar_selection_visible();
    }

    pub fn collapse_current_node(&mut self) {
        self.ensure_sidebar_selection_visible();
    }

    pub fn expand_current_node(&mut self) {
        self.ensure_sidebar_selection_visible();
    }

    pub fn collapse_all(&mut self) {
        self.ensure_sidebar_selection_visible();
    }

    pub fn expand_all(&mut self) {
        self.ensure_sidebar_selection_visible();
    }

    pub fn scroll_lines(&mut self, delta: isize) {
        self.ensure_current_diff_layout();
        let max = self.max_scroll();
        if delta > 0 && self.scroll >= max {
            self.scroll = max;
            return;
        }
        let next = (self.scroll as i128 + delta as i128).clamp(0, max as i128) as usize;
        self.scroll = next;
    }

    fn rebuild_sidebar_labels(&mut self) {
        self.sidebar_labels = abbreviate_sidebar_paths(&self.files);
        self.refresh_sidebar_search_matches();
        self.sidebar_width =
            compute_sidebar_width(self.terminal_width, &self.visible_sidebar_entries());
        self.committed_file = self.committed_file.min(self.files.len().saturating_sub(1));
        self.preview_file = self
            .preview_file
            .map(|index| index.min(self.files.len().saturating_sub(1)));
        self.ensure_sidebar_selection_visible();
    }

    fn move_sidebar_selection(&mut self, delta: isize) {
        if let Some(state) = self.sidebar_search.as_mut() {
            if state.matches.is_empty() {
                return;
            }
            let max = state.matches.len().saturating_sub(1);
            let next = (state.cursor as i128 + delta as i128).clamp(0, max as i128) as usize;
            state.cursor = next;
            self.preview_sidebar_search_selection();
            self.ensure_sidebar_selection_visible();
        } else {
            let entries = self.visible_sidebar_entries();
            if entries.is_empty() {
                return;
            }
            let current = self
                .current_sidebar_entry_index()
                .unwrap_or(self.committed_file.min(entries.len().saturating_sub(1)));
            let max = entries.len().saturating_sub(1);
            let next = (current as i128 + delta as i128).clamp(0, max as i128) as usize;
            self.select_sidebar_entry_at(next);
        }
    }

    fn select_sidebar_entry_at(&mut self, index: usize) {
        let entries = self.visible_sidebar_entries();
        let Some(entry) = entries.get(index) else {
            return;
        };

        self.commit_file_by_index(entry.file_index);
    }

    fn current_sidebar_entry_index(&self) -> Option<usize> {
        let current = self.current_sidebar_file_index()?;
        self.visible_sidebar_entries()
            .iter()
            .position(|entry| entry.file_index == current)
    }

    fn ensure_sidebar_selection_visible(&mut self) {
        let entries = self.visible_sidebar_entries();
        let Some(index) = self.current_sidebar_entry_index() else {
            self.sidebar_scroll = 0;
            return;
        };

        if index < self.sidebar_scroll {
            self.sidebar_scroll = index;
        }

        let viewport = self.sidebar_viewport_height.max(1);
        if index >= self.sidebar_scroll + viewport {
            self.sidebar_scroll = index + 1 - viewport;
        }

        let max_scroll = entries.len().saturating_sub(viewport);
        self.sidebar_scroll = self.sidebar_scroll.min(max_scroll);
    }

    fn commit_file_by_index(&mut self, index: usize) {
        if self.files.is_empty() {
            return;
        }

        let next = index.min(self.files.len() - 1);
        self.set_active_file(Some(next), true);
        self.ensure_sidebar_selection_visible();
    }

    fn visible_file_indices(&self) -> Vec<(usize, &FileDiff)> {
        match &self.sidebar_search {
            Some(state) => state
                .matches
                .iter()
                .filter_map(|&index| self.files.get(index).map(|file| (index, file)))
                .collect(),
            None => self.files.iter().enumerate().collect(),
        }
    }

    fn current_sidebar_file_index(&self) -> Option<usize> {
        match &self.sidebar_search {
            Some(state) => state.matches.get(state.cursor).copied(),
            None => Some(self.committed_file),
        }
    }

    fn refresh_sidebar_search_matches(&mut self) {
        let current_active = self.active_file_index();
        let Some(state) = self.sidebar_search.as_mut() else {
            return;
        };

        let previous = state
            .matches
            .get(state.cursor)
            .copied()
            .or(Some(current_active));
        state.matches = compute_sidebar_search_matches(&self.files, &state.query);
        state.cursor = previous
            .and_then(|index| {
                state
                    .matches
                    .iter()
                    .position(|candidate| *candidate == index)
            })
            .unwrap_or(0);
        if !state.matches.is_empty() {
            state.cursor = state.cursor.min(state.matches.len() - 1);
        }
        self.preview_sidebar_search_selection();
        self.sidebar_width =
            compute_sidebar_width(self.terminal_width, &self.visible_sidebar_entries());
        self.ensure_sidebar_selection_visible();
    }

    fn preview_sidebar_search_selection(&mut self) {
        if let Some(index) = self.current_sidebar_file_index() {
            self.set_active_file(Some(index), false);
        } else {
            self.preview_file = None;
        }
    }

    fn active_file_index(&self) -> usize {
        self.preview_file.unwrap_or(self.committed_file)
    }

    fn set_active_file(&mut self, index: Option<usize>, commit: bool) {
        let previous = self.active_file_index();
        match (index, commit) {
            (Some(next), true) => {
                self.committed_file = next;
                self.preview_file = None;
            }
            (Some(next), false) => {
                self.preview_file = Some(next);
            }
            (None, _) => {
                self.preview_file = None;
            }
        }

        if self.active_file_index() != previous {
            self.scroll = 0;
            self.active_diff_layout = None;
        }
    }

    fn display_path_for_sidebar_entry(&self, file_index: usize, file: &FileDiff) -> String {
        if self
            .sidebar_entry_match_positions(file_index, file)
            .is_empty()
        {
            self.sidebar_labels
                .get(file_index)
                .cloned()
                .unwrap_or_else(|| file.path.clone())
        } else {
            file.path.clone()
        }
    }

    fn sidebar_entry_match_positions(&self, _file_index: usize, file: &FileDiff) -> Vec<usize> {
        self.sidebar_search
            .as_ref()
            .and_then(|state| fuzzy_match_positions(&file.path, &state.query))
            .unwrap_or_default()
    }

    fn max_scroll(&self) -> usize {
        let Some(layout) = self.current_diff_layout() else {
            return 0;
        };
        layout
            .visual_line_count()
            .saturating_sub(self.diff_viewport_height)
    }

    fn max_anchor_scroll(&self) -> usize {
        let Some(layout) = self.current_diff_layout() else {
            return 0;
        };
        layout.visual_line_count().saturating_sub(1)
    }

    fn clamp_scroll(&mut self) {
        self.ensure_current_diff_layout();
        self.scroll = self.scroll.min(self.max_anchor_scroll());
    }

    fn current_diff_layout(&self) -> Option<&DiffLayout> {
        let file = self.current_file()?;
        let active = self.active_diff_layout.as_ref()?;
        (active.file_path == file.path).then_some(&active.layout)
    }

    fn ensure_current_diff_layout(&mut self) {
        let Some(file) = self.current_file() else {
            self.active_diff_layout = None;
            return;
        };

        let needs_refresh = self.active_diff_layout.as_ref().is_none_or(|active| {
            active.file_path != file.path
                || active.layout.content_width() != self.diff_content_width
                || active.layout.tab_width() != self.tab_width
        });

        if needs_refresh {
            self.active_diff_layout = Some(ActiveDiffLayout {
                file_path: file.path.clone(),
                layout: DiffLayout::with_tab_width(file, self.diff_content_width, self.tab_width),
            });
        }
    }
}

fn compute_sidebar_search_matches(files: &[FileDiff], query: &str) -> Vec<usize> {
    if query.is_empty() {
        return (0..files.len()).collect();
    }

    let mut matches = files
        .iter()
        .enumerate()
        .filter_map(|(index, file)| {
            fuzzy_match_score(&file.path, query)
                .or_else(|| {
                    file.old_path
                        .as_deref()
                        .and_then(|path| fuzzy_match_score(path, query))
                })
                .map(|score| (index, score))
        })
        .collect::<Vec<_>>();
    matches.sort_by(|lhs, rhs| rhs.1.cmp(&lhs.1).then_with(|| lhs.0.cmp(&rhs.0)));
    matches.into_iter().map(|(index, _)| index).collect()
}

fn fuzzy_match_score(candidate: &str, query: &str) -> Option<i64> {
    let positions = fuzzy_match_positions(candidate, query)?;
    let candidate = candidate.to_ascii_lowercase();
    let candidate_chars = candidate.chars().collect::<Vec<_>>();

    let mut score = 0_i64;
    let mut consecutive_bonus = 0_i64;
    for (idx, pos) in positions.iter().copied().enumerate() {
        score += 10;
        if idx == 0 && pos == 0 {
            score += 25;
        }
        if idx > 0 && positions[idx - 1] + 1 == pos {
            consecutive_bonus += 1;
            score += 18 + consecutive_bonus * 4;
        } else {
            consecutive_bonus = 0;
        }
        if pos == 0
            || matches!(
                candidate_chars.get(pos.saturating_sub(1)),
                Some('/' | '_' | '-' | '.')
            )
        {
            score += 20;
        }
    }

    let spread = positions.last().copied().unwrap_or(0) - positions[0];
    score -= spread as i64;
    score -= candidate_chars.len() as i64 / 3;
    Some(score)
}

fn fuzzy_match_positions(candidate: &str, query: &str) -> Option<Vec<usize>> {
    if query.is_empty() {
        return Some(Vec::new());
    }

    let candidate = candidate.to_ascii_lowercase();
    let query = query.to_ascii_lowercase();
    let candidate_chars = candidate.chars().collect::<Vec<_>>();
    let mut positions = Vec::with_capacity(query.len());
    let mut search_from = 0;

    for needle in query.chars() {
        let found = candidate_chars
            .iter()
            .enumerate()
            .skip(search_from)
            .find_map(|(idx, ch)| (*ch == needle).then_some(idx))?;
        positions.push(found);
        search_from = found + 1;
    }

    Some(positions)
}

fn compute_sidebar_width(terminal_width: u16, entries: &[SidebarEntry]) -> u16 {
    let desired = entries
        .iter()
        .map(|entry| UnicodeWidthStr::width(entry.label().as_str()) as u16)
        .max()
        .unwrap_or(0)
        .saturating_add(2);
    let min_width = terminal_width.saturating_sub(20).min(16);
    let max_width = terminal_width
        .saturating_sub(20)
        .clamp(min_width.max(1), 80);
    desired.clamp(min_width.max(1), max_width)
}

fn abbreviate_sidebar_paths(files: &[FileDiff]) -> Vec<String> {
    let mut siblings_by_parent = HashMap::<String, Vec<String>>::new();

    for file in files {
        let parts = path_parts(&file.path);
        let mut parent = String::new();
        for segment in parts.directories {
            siblings_by_parent
                .entry(parent.clone())
                .or_default()
                .push(segment.to_string());
            push_path_part(&mut parent, segment);
        }
    }

    for siblings in siblings_by_parent.values_mut() {
        siblings.sort();
        siblings.dedup();
    }

    files
        .iter()
        .map(|file| abbreviate_single_path(&file.path, &siblings_by_parent))
        .collect()
}

fn abbreviate_single_path(path: &str, siblings_by_parent: &HashMap<String, Vec<String>>) -> String {
    let parts = path_parts(path);
    let mut parent = String::new();
    let mut abbreviated = Vec::with_capacity(parts.directories.len() + 1);

    for segment in parts.directories {
        let short = siblings_by_parent
            .get(&parent)
            .map(|siblings| shortest_unique_prefix(segment, siblings))
            .unwrap_or_else(|| segment.to_string());
        abbreviated.push(short);
        push_path_part(&mut parent, segment);
    }

    abbreviated.push(parts.file_name.to_string());
    abbreviated.join("/")
}

fn shortest_unique_prefix(name: &str, siblings: &[String]) -> String {
    for len in 1..=name.chars().count() {
        let prefix = name.chars().take(len).collect::<String>();
        if siblings
            .iter()
            .filter(|candidate| candidate.starts_with(&prefix))
            .count()
            == 1
        {
            return prefix;
        }
    }

    name.to_string()
}

fn push_path_part(path: &mut String, segment: &str) {
    if !path.is_empty() {
        path.push('/');
    }
    path.push_str(segment);
}

struct PathParts<'a> {
    directories: Vec<&'a str>,
    file_name: &'a str,
}

fn path_parts(path: &str) -> PathParts<'_> {
    let mut parts = path.split('/').collect::<Vec<_>>();
    let file_name = parts.pop().unwrap_or(path);
    PathParts {
        directories: parts,
        file_name,
    }
}

#[derive(Debug)]
struct RenderedDiff {
    lines: Vec<Line<'static>>,
    hunk_starts: Vec<usize>,
}

fn render_file(file: &FileDiff, content_width: usize, tab_width: usize) -> RenderedDiff {
    let backgrounds = FileBackgrounds::from_file(file, tab_width);
    let mut lines = Vec::new();
    let mut hunk_starts = Vec::new();

    for meta in &file.metadata_lines {
        lines.push(Line::from(vec![Span::styled(meta.clone(), dimmed_style())]));
    }
    if let Some(reason) = &file.line_diff_reason {
        lines.push(Line::from(Span::styled(
            format!("Line diff: {reason}."),
            dimmed_style(),
        )));
    }
    if !lines.is_empty() && !file.hunks.is_empty() {
        lines.push(Line::default());
    }

    let lhs_gutter = max_line_number(file, true).to_string().len().max(1);
    let rhs_gutter = max_line_number(file, false).to_string().len().max(1);

    for (idx, hunk) in file.hunks.iter().enumerate() {
        if idx > 0 {
            lines.push(Line::default());
        }
        hunk_starts.push(lines.len());
        if let Some(title) = &hunk.title {
            lines.push(render_hunk_title(title));
        }
        let rendered = render_hunk(
            file,
            hunk,
            content_width,
            lhs_gutter,
            rhs_gutter,
            tab_width,
            &backgrounds,
        );
        lines.extend(rendered);
    }

    if lines.is_empty() {
        lines.push(Line::from("(empty diff output)"));
    }

    RenderedDiff { lines, hunk_starts }
}

fn render_hunk(
    file: &FileDiff,
    hunk: &DiffHunk,
    content_width: usize,
    lhs_gutter: usize,
    rhs_gutter: usize,
    tab_width: usize,
    backgrounds: &FileBackgrounds,
) -> Vec<Line<'static>> {
    let mut changed_on_lhs = false;
    let mut changed_on_rhs = false;
    for row in &hunk.rows {
        if row.omitted {
            continue;
        }
        if row
            .lhs
            .as_ref()
            .is_some_and(|side| !side.diff_ranges.is_empty())
        {
            changed_on_lhs = true;
        }
        if row
            .rhs
            .as_ref()
            .is_some_and(|side| !side.diff_ranges.is_empty())
        {
            changed_on_rhs = true;
        }
    }

    let rhs_only = changed_on_rhs && !changed_on_lhs;
    let lhs_only = changed_on_lhs && !changed_on_rhs;

    if rhs_only {
        return render_single_column(
            file,
            &hunk.rows,
            content_width,
            rhs_gutter,
            SideKind::Right,
            tab_width,
            backgrounds,
        );
    }
    if lhs_only {
        return render_single_column(
            file,
            &hunk.rows,
            content_width,
            lhs_gutter,
            SideKind::Left,
            tab_width,
            backgrounds,
        );
    }

    render_two_columns(
        file,
        hunk,
        content_width,
        lhs_gutter,
        rhs_gutter,
        tab_width,
        backgrounds,
    )
}

fn render_hunk_title(title: &str) -> Line<'static> {
    if let Some((head, tail)) = title.split_once(" --- ") {
        let mut spans = vec![Span::styled(head.to_string(), dimmed_style())];
        spans.push(Span::styled(format!(" --- {tail}"), dimmed_style()));
        Line::from(spans)
    } else {
        Line::from(vec![Span::styled(title.to_string(), dimmed_style())])
    }
}

fn render_single_column(
    file: &FileDiff,
    rows: &[DiffRow],
    content_width: usize,
    gutter_width: usize,
    side_kind: SideKind,
    tab_width: usize,
    backgrounds: &FileBackgrounds,
) -> Vec<Line<'static>> {
    let shared_indent = shared_hunk_indent(rows, side_kind);
    let mut lines = Vec::new();
    for row in rows {
        if row.omitted {
            lines.push(omitted_single_column_line(content_width));
            continue;
        }

        let side = match side_kind {
            SideKind::Left => row.lhs.as_ref(),
            SideKind::Right => row.rhs.as_ref(),
        };
        if let Some(side) = side {
            let wrapped = render_side(
                file,
                side,
                SideLayout {
                    total_width: content_width,
                    gutter_width,
                    side_kind,
                    shared_indent,
                    tab_width,
                    backgrounds,
                },
            );
            lines.extend(wrapped);
        }
    }
    lines
}

fn render_two_columns(
    file: &FileDiff,
    hunk: &DiffHunk,
    content_width: usize,
    lhs_gutter: usize,
    rhs_gutter: usize,
    tab_width: usize,
    backgrounds: &FileBackgrounds,
) -> Vec<Line<'static>> {
    let separator = "  ";
    let separator_width = UnicodeWidthStr::width(separator);
    let available = content_width.saturating_sub(separator_width).max(1);
    let lhs_width = available / 2;
    let rhs_width = available.saturating_sub(lhs_width);
    let lhs_shared_indent = shared_hunk_indent(&hunk.rows, SideKind::Left);
    let rhs_shared_indent = shared_hunk_indent(&hunk.rows, SideKind::Right);

    let mut lines = Vec::new();

    for row in &hunk.rows {
        if row.omitted {
            lines.push(omitted_two_column_line(lhs_width, rhs_width));
            continue;
        }
        let lhs = row
            .lhs
            .as_ref()
            .map(|side| {
                render_side(
                    file,
                    side,
                    SideLayout {
                        total_width: lhs_width,
                        gutter_width: lhs_gutter,
                        side_kind: SideKind::Left,
                        shared_indent: lhs_shared_indent,
                        tab_width,
                        backgrounds,
                    },
                )
            })
            .unwrap_or_else(|| vec![blank_side_line(lhs_width)]);
        let rhs = row
            .rhs
            .as_ref()
            .map(|side| {
                render_side(
                    file,
                    side,
                    SideLayout {
                        total_width: rhs_width,
                        gutter_width: rhs_gutter,
                        side_kind: SideKind::Right,
                        shared_indent: rhs_shared_indent,
                        tab_width,
                        backgrounds,
                    },
                )
            })
            .unwrap_or_else(|| vec![blank_side_line(rhs_width)]);

        for idx in 0..lhs.len().max(rhs.len()) {
            let mut spans = Vec::new();
            spans.extend(
                lhs.get(idx)
                    .cloned()
                    .unwrap_or_else(|| blank_side_line(lhs_width))
                    .spans,
            );
            spans.push(Span::raw(separator.to_string()));
            spans.extend(
                rhs.get(idx)
                    .cloned()
                    .unwrap_or_else(|| blank_side_line(rhs_width))
                    .spans,
            );
            lines.push(Line::from(spans));
        }
    }

    lines
}

fn blank_side_line(width: usize) -> Line<'static> {
    Line::from(vec![Span::raw(" ".repeat(width))])
}

fn omitted_two_column_line(lhs_width: usize, rhs_width: usize) -> Line<'static> {
    let lhs = format!("{:>3} {}", "...", " ".repeat(lhs_width.saturating_sub(4)));
    let rhs = format!("{:>3} {}", "...", " ".repeat(rhs_width.saturating_sub(4)));
    Line::from(vec![
        Span::styled(lhs, dimmed_style()),
        Span::raw("  ".to_string()),
        Span::styled(rhs, dimmed_style()),
    ])
}

fn omitted_single_column_line(width: usize) -> Line<'static> {
    let marker = format!("{:>3} {}", "...", " ".repeat(width.saturating_sub(4)));
    Line::from(vec![Span::styled(marker, dimmed_style())])
}

struct SideLayout<'a> {
    total_width: usize,
    gutter_width: usize,
    side_kind: SideKind,
    shared_indent: usize,
    tab_width: usize,
    backgrounds: &'a FileBackgrounds,
}

fn render_side(file: &FileDiff, side: &DiffSide, layout: SideLayout<'_>) -> Vec<Line<'static>> {
    let SideLayout {
        total_width,
        gutter_width,
        side_kind,
        shared_indent,
        tab_width,
        backgrounds,
    } = layout;
    let content_width = total_width.saturating_sub(gutter_width + 1).max(1);
    let mut content = styled_content(file, side, side_kind, shared_indent);
    let mut wrapped = wrap_segments(&mut content, content_width, tab_width, shared_indent);

    if wrapped.is_empty() {
        wrapped.push(Vec::new());
    }
    let background_runs = backgrounds.line(side_kind, side.line_number);
    paint_nested_backgrounds(&mut wrapped, background_runs, shared_indent, content_width);

    let mut lines = Vec::new();
    let changed = !side.diff_ranges.is_empty();
    for (idx, spans) in wrapped.into_iter().enumerate() {
        let gutter = if idx == 0 {
            format!("{:>width$} ", side.line_number, width = gutter_width)
        } else {
            format!("{:>width$} ", "", width = gutter_width)
        };
        let gutter_style = if changed && idx == 0 {
            Style::default()
                .fg(color_for_diff(side_kind))
                .bg(diff_gutter_bg_color(side_kind))
        } else {
            dimmed_style()
        };
        let mut line_spans = vec![Span::styled(gutter, gutter_style)];
        if spans.is_empty() {
            line_spans.push(Span::raw(" ".repeat(content_width)));
        } else {
            line_spans.extend(spans);
            let visible = visible_span_width(&line_spans[1..]);
            if visible < content_width {
                line_spans.push(Span::raw(" ".repeat(content_width - visible)));
            }
        }
        lines.push(Line::from(line_spans));
    }

    lines
}

fn paint_nested_backgrounds(
    wrapped: &mut [Vec<Span<'static>>],
    runs: &[kat::HighlightBackgroundRun],
    shared_indent: usize,
    content_width: usize,
) {
    if runs.is_empty() {
        return;
    }

    let mut line_offset = 0;
    for (row_index, spans) in wrapped.iter_mut().enumerate() {
        let row_width = visible_span_width(spans);
        let mut painted = Vec::<Span<'static>>::new();
        let mut column = 0;
        for span in spans.iter() {
            for grapheme in span.content.graphemes(true) {
                let width = UnicodeWidthStr::width(grapheme);
                let source_column = shared_indent + line_offset + column;
                let mut style = span.style;
                if style.bg.is_none()
                    && let Some(run) = runs.iter().find(|run| {
                        run.start_column <= source_column && source_column < run.end_column
                    })
                {
                    let (r, g, b) = run.background;
                    style = style.bg(Color::Rgb(r, g, b));
                }
                push_styled_grapheme(&mut painted, grapheme, style);
                column += width;
            }
        }

        let source_end = shared_indent + line_offset + row_width;
        let extension = runs
            .iter()
            .filter(|run| (row_width == 0 && row_index == 0) || run.start_column <= source_end)
            .map(|run| run.end_column.saturating_sub(shared_indent + line_offset))
            .max()
            .unwrap_or(row_width)
            .min(content_width);
        for column in row_width..extension {
            let source_column = shared_indent + line_offset + column;
            let style = runs
                .iter()
                .find(|run| run.start_column <= source_column && source_column < run.end_column)
                .map_or_else(Style::default, |run| {
                    let (r, g, b) = run.background;
                    Style::default().bg(Color::Rgb(r, g, b))
                });
            push_styled_grapheme(&mut painted, " ", style);
        }
        *spans = painted;
        line_offset += row_width;
    }
}

fn push_styled_grapheme(spans: &mut Vec<Span<'static>>, grapheme: &str, style: Style) {
    if let Some(last) = spans.last_mut()
        && last.style == style
    {
        last.content.to_mut().push_str(grapheme);
    } else {
        spans.push(Span::styled(grapheme.to_owned(), style));
    }
}

fn styled_content(
    file: &FileDiff,
    side: &DiffSide,
    side_kind: SideKind,
    shared_indent: usize,
) -> Vec<StyledSpan> {
    let syntax_ranges = file.syntax_for_side(side_kind, side.line_number);
    let (display_text, syntax_ranges, diff_ranges) =
        trim_shared_indent(&side.text, syntax_ranges, &side.diff_ranges, shared_indent);
    let display_diff_ranges = merge_display_diff_ranges(&display_text, &diff_ranges);
    let boundaries = collect_boundaries(
        &display_text,
        &syntax_ranges,
        &diff_ranges,
        &display_diff_ranges,
    );
    if boundaries.len() <= 1 {
        return vec![StyledSpan {
            content: display_text,
            style: Style::default(),
        }];
    }

    let mut spans = Vec::new();
    for window in boundaries.windows(2) {
        let start = window[0];
        let end = window[1];
        if start == end {
            continue;
        }
        let Some(segment) = display_text.get(start..end) else {
            continue;
        };
        let mut style = Style::default();
        let syntax_style = syntax_ranges
            .iter()
            .find(|range| range.start <= start && end <= range.end)
            .map(|range| range.style);
        if let Some(syntax) = syntax_style {
            style = apply_syntax_style(style, syntax);
        }

        if display_diff_ranges
            .iter()
            .any(|range| range.start <= start && end <= range.end)
        {
            style = style.bg(inline_diff_bg_color(side_kind));
        }

        spans.push(StyledSpan {
            content: segment.to_string(),
            style,
        });
    }

    spans
}

fn shared_hunk_indent(rows: &[DiffRow], side_kind: SideKind) -> usize {
    const MIN_SHARED_INDENT: usize = 5;

    let mut min_indent = usize::MAX;
    let mut saw_non_empty_line = false;

    for row in rows {
        if row.omitted {
            continue;
        }
        let side = match side_kind {
            SideKind::Left => row.lhs.as_ref(),
            SideKind::Right => row.rhs.as_ref(),
        };
        let Some(side) = side else {
            continue;
        };
        if side.text.is_empty() {
            continue;
        }

        let leading_spaces = side.text.bytes().take_while(|b| *b == b' ').count();
        if leading_spaces < MIN_SHARED_INDENT
            || side
                .diff_ranges
                .iter()
                .any(|range| range.start < leading_spaces)
        {
            return 0;
        }

        min_indent = min_indent.min(leading_spaces);
        saw_non_empty_line = true;
    }

    if saw_non_empty_line { min_indent } else { 0 }
}

fn trim_shared_indent(
    text: &str,
    syntax_ranges: &[kat_adapter::SyntaxRange],
    diff_ranges: &[DiffRange],
    shared_indent: usize,
) -> (String, Vec<kat_adapter::SyntaxRange>, Vec<DiffRange>) {
    if shared_indent == 0 || text.is_empty() {
        return (
            text.to_string(),
            syntax_ranges.to_vec(),
            diff_ranges.to_vec(),
        );
    }

    let trimmed = text.get(shared_indent..).unwrap_or("").to_string();
    let shift = -(shared_indent as isize);

    (
        trimmed,
        remap_syntax_ranges(syntax_ranges, shared_indent, shift, 0),
        remap_diff_ranges(diff_ranges, shared_indent, shift, 0),
    )
}

fn remap_syntax_ranges(
    ranges: &[kat_adapter::SyntaxRange],
    compressed_until: usize,
    shift: isize,
    prefix_len: usize,
) -> Vec<kat_adapter::SyntaxRange> {
    ranges
        .iter()
        .filter_map(|range| {
            if range.end <= compressed_until {
                return None;
            }
            let start = if range.start < compressed_until {
                prefix_len
            } else {
                ((range.start as isize) + shift) as usize
            };
            let end = ((range.end as isize) + shift) as usize;
            (start < end).then_some(kat_adapter::SyntaxRange {
                start,
                end,
                style: range.style,
            })
        })
        .collect()
}

fn remap_diff_ranges(
    ranges: &[DiffRange],
    compressed_until: usize,
    shift: isize,
    prefix_len: usize,
) -> Vec<DiffRange> {
    ranges
        .iter()
        .filter_map(|range| {
            if range.end <= compressed_until {
                return None;
            }
            let start = if range.start < compressed_until {
                prefix_len
            } else {
                ((range.start as isize) + shift) as usize
            };
            let end = ((range.end as isize) + shift) as usize;
            (start < end).then_some(DiffRange {
                start,
                end,
                kind: range.kind,
            })
        })
        .collect()
}

fn collect_boundaries(
    text: &str,
    syntax_ranges: &[kat_adapter::SyntaxRange],
    diff_ranges: &[DiffRange],
    display_diff_ranges: &[DiffRange],
) -> Vec<usize> {
    let mut boundaries = vec![0, text.len()];
    boundaries.extend(
        syntax_ranges
            .iter()
            .flat_map(|range| [range.start, range.end]),
    );
    boundaries.extend(
        diff_ranges
            .iter()
            .flat_map(|range| [range.start, range.end]),
    );
    boundaries.extend(
        display_diff_ranges
            .iter()
            .flat_map(|range| [range.start, range.end]),
    );
    boundaries.sort_unstable();
    boundaries.dedup();
    boundaries
        .into_iter()
        .filter(|idx| text.is_char_boundary(*idx))
        .collect()
}

fn merge_display_diff_ranges(text: &str, diff_ranges: &[DiffRange]) -> Vec<DiffRange> {
    if diff_ranges.is_empty() {
        return Vec::new();
    }

    let mut merged = Vec::with_capacity(diff_ranges.len());
    let mut current = diff_ranges[0].clone();

    for range in &diff_ranges[1..] {
        let whitespace_gap = text
            .get(current.end..range.start)
            .is_some_and(|gap| !gap.is_empty() && gap.chars().all(char::is_whitespace));

        if range.start <= current.end || whitespace_gap {
            current.end = current.end.max(range.end);
            continue;
        }

        merged.push(current);
        current = range.clone();
    }

    merged.push(current);
    merged
}

#[derive(Debug, Clone)]
struct StyledSpan {
    content: String,
    style: Style,
}

fn wrap_segments(
    segments: &mut [StyledSpan],
    width: usize,
    tab_width: usize,
    start_column: usize,
) -> Vec<Vec<Span<'static>>> {
    let mut lines: Vec<Vec<Span<'static>>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut current_width = 0;
    let mut source_column = start_column;

    for segment in segments {
        let mut remaining = segment.content.as_str();
        while !remaining.is_empty() {
            let available = width.saturating_sub(current_width);
            if available == 0 {
                lines.push(current);
                current = Vec::new();
                current_width = 0;
                continue;
            }

            let first_grapheme = remaining.graphemes(true).next().expect("non-empty");
            if current_width > 0
                && display_width_for_grapheme(first_grapheme, source_column, tab_width) > available
            {
                lines.push(current);
                current = Vec::new();
                current_width = 0;
                continue;
            }

            let (head, tail, used_width) =
                split_by_width(remaining, available, source_column, tab_width);
            if !head.is_empty() {
                current.push(Span::styled(
                    expand_tabs_for_display(head, source_column, tab_width),
                    segment.style,
                ));
                current_width += used_width;
                source_column += used_width;
            }
            remaining = tail;

            if current_width >= width {
                lines.push(current);
                current = Vec::new();
                current_width = 0;
            }
        }
    }

    if !current.is_empty() {
        lines.push(current);
    }

    lines
}

fn split_by_width(
    input: &str,
    width: usize,
    start_col: usize,
    tab_width: usize,
) -> (&str, &str, usize) {
    if input.is_empty() {
        return ("", "", 0);
    }

    let mut used_bytes = 0;
    let mut used_width = 0;
    let mut current_col = start_col;
    for grapheme in input.graphemes(true) {
        let grapheme_width = display_width_for_grapheme(grapheme, current_col, tab_width);
        if used_width + grapheme_width > width && used_bytes > 0 {
            break;
        }
        used_bytes += grapheme.len();
        used_width += grapheme_width;
        current_col += grapheme_width;
        if used_width >= width {
            break;
        }
    }

    if used_bytes == 0 {
        let grapheme = input.graphemes(true).next().expect("non-empty");
        let used = grapheme.len();
        return (
            &input[..used],
            &input[used..],
            display_width_for_grapheme(grapheme, start_col, tab_width),
        );
    }

    (&input[..used_bytes], &input[used_bytes..], used_width)
}

fn display_width_for_grapheme(grapheme: &str, current_col: usize, tab_width: usize) -> usize {
    if grapheme == "\t" {
        tab_width - (current_col % tab_width)
    } else {
        UnicodeWidthStr::width(grapheme)
    }
}

fn expand_tabs_for_display(input: &str, start_col: usize, tab_width: usize) -> String {
    if !input.contains('\t') {
        return input.to_string();
    }

    let mut rendered = String::with_capacity(input.len());
    let mut current_col = start_col;
    for grapheme in input.graphemes(true) {
        if grapheme == "\t" {
            let spaces = tab_width - (current_col % tab_width);
            rendered.push_str(&" ".repeat(spaces));
            current_col += spaces;
        } else {
            rendered.push_str(grapheme);
            current_col += UnicodeWidthStr::width(grapheme);
        }
    }
    rendered
}

fn max_line_number(file: &FileDiff, lhs: bool) -> usize {
    file.hunks
        .iter()
        .flat_map(|hunk| hunk.rows.iter())
        .filter_map(|row| {
            if lhs {
                row.lhs.as_ref().map(|side| side.line_number)
            } else {
                row.rhs.as_ref().map(|side| side.line_number)
            }
        })
        .max()
        .unwrap_or(1)
}

pub(crate) fn visible_span_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|span| span.content.width()).sum()
}

fn color_for_diff(side_kind: SideKind) -> Color {
    match side_kind {
        SideKind::Left => Color::Rgb(255, 85, 85),
        SideKind::Right => Color::Rgb(80, 250, 123),
    }
}

fn inline_diff_bg_color(side_kind: SideKind) -> Color {
    match side_kind {
        SideKind::Left => blend_rgb(dracula_bg(), (255, 85, 85), 0.34),
        SideKind::Right => blend_rgb(dracula_bg(), (80, 250, 123), 0.24),
    }
}

fn diff_gutter_bg_color(side_kind: SideKind) -> Color {
    match side_kind {
        SideKind::Left => blend_rgb(dracula_bg(), (255, 85, 85), 0.22),
        SideKind::Right => blend_rgb(dracula_bg(), (80, 250, 123), 0.18),
    }
}

fn apply_syntax_style(style: Style, syntax: kat::HighlightStyle) -> Style {
    let (r, g, b) = syntax.foreground;
    let mut modifiers = Modifier::empty();
    if syntax.bold {
        modifiers |= Modifier::BOLD;
    }
    if syntax.italic {
        modifiers |= Modifier::ITALIC;
    }
    if syntax.underline {
        modifiers |= Modifier::UNDERLINED;
    }
    if syntax.strikethrough {
        modifiers |= Modifier::CROSSED_OUT;
    }
    style.fg(Color::Rgb(r, g, b)).add_modifier(modifiers)
}

pub(crate) fn dimmed_style() -> Style {
    Style::default().fg(muted_gray())
}

pub(crate) fn dracula_bg() -> (u8, u8, u8) {
    (40, 42, 54)
}

fn blend_rgb(base: (u8, u8, u8), accent: (u8, u8, u8), ratio: f32) -> Color {
    let mix = |bg: u8, fg: u8| -> u8 {
        (((bg as f32) * (1.0 - ratio)) + ((fg as f32) * ratio)).round() as u8
    };
    Color::Rgb(
        mix(base.0, accent.0),
        mix(base.1, accent.1),
        mix(base.2, accent.2),
    )
}

fn muted_gray() -> Color {
    Color::Rgb(98, 114, 164)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SideKind {
    Left,
    Right,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn syntax_style(foreground: (u8, u8, u8)) -> kat::HighlightStyle {
        kat::HighlightStyle {
            foreground,
            bold: false,
            italic: false,
            underline: false,
            strikethrough: false,
        }
    }

    #[test]
    fn file_display_name_shows_rename() {
        let file = sample_file();
        assert_eq!(file.display_name(), "[R] old.rs -> new.rs");
    }

    #[test]
    fn diff_layout_tracks_hunk_starts() {
        let file = FileDiff {
            path: "x.rs".to_string(),
            old_path: None,
            status: DiffStatus::Modified,
            line_diff_reason: None,
            metadata_lines: vec![],
            hunks: vec![
                DiffHunk {
                    title: Some("x.rs --- 1/2 --- Rust".to_string()),
                    rows: vec![DiffRow::new(
                        Some(DiffSide {
                            line_number: 1,
                            text: "let a = 1;".to_string(),
                            diff_ranges: vec![DiffRange {
                                start: 8,
                                end: 9,
                                kind: DiffHighlightKind::Keyword,
                            }],
                        }),
                        Some(DiffSide {
                            line_number: 1,
                            text: "let a = 2;".to_string(),
                            diff_ranges: vec![DiffRange {
                                start: 8,
                                end: 9,
                                kind: DiffHighlightKind::Keyword,
                            }],
                        }),
                    )],
                },
                DiffHunk {
                    title: Some("x.rs --- 2/2 --- Rust".to_string()),
                    rows: vec![DiffRow::new(
                        None,
                        Some(DiffSide {
                            line_number: 8,
                            text: "added();".to_string(),
                            diff_ranges: vec![DiffRange {
                                start: 0,
                                end: 8,
                                kind: DiffHighlightKind::Normal,
                            }],
                        }),
                    )],
                },
            ],
            lhs_syntax: vec![vec![]; 8],
            rhs_syntax: vec![vec![]; 8],
            lhs_document: None,
            rhs_document: None,
        };

        let layout = DiffLayout::build(&file, 40);
        assert_eq!(layout.visual_hunk_starts(), &[0, 3]);
        assert!(layout.visual_line_count() >= 4);
    }

    #[test]
    fn jump_bottom_uses_viewport_height() {
        let file = big_file();
        let mut app = AppState::new(vec![file], 120);
        app.update_ui_layout(UiLayout {
            sidebar_inner: Rect::new(0, 1, 32, 12),
            diff_area: Rect::new(33, 1, 87, 12),
            diff_inner: Rect::new(33, 1, 87, 12),
        });

        app.route = Route::Diff;
        app.jump_bottom();
        assert!(app.scroll > 0);
    }

    #[test]
    fn inline_diff_uses_background_and_preserves_syntax_foreground() {
        let file = FileDiff {
            path: "x.rs".to_string(),
            old_path: None,
            status: DiffStatus::Modified,
            line_diff_reason: None,
            metadata_lines: vec![],
            hunks: vec![],
            lhs_syntax: vec![vec![kat_adapter::SyntaxRange {
                start: 0,
                end: 5,
                style: syntax_style((241, 250, 140)),
            }]],
            rhs_syntax: vec![],
            lhs_document: None,
            rhs_document: None,
        };
        let side = DiffSide {
            line_number: 1,
            text: "\"old\"".to_string(),
            diff_ranges: vec![DiffRange {
                start: 0,
                end: 5,
                kind: DiffHighlightKind::Normal,
            }],
        };

        let spans = styled_content(&file, &side, SideKind::Left, 0);

        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "\"old\"");
        assert_eq!(spans[0].style.fg, Some(Color::Rgb(241, 250, 140)));
        assert_eq!(
            spans[0].style.bg,
            Some(inline_diff_bg_color(SideKind::Left))
        );
        assert!(!spans[0].style.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn inserted_inline_diff_preserves_dracula_syntax_foreground() {
        let file = FileDiff {
            path: "x.rs".to_string(),
            old_path: None,
            status: DiffStatus::Modified,
            line_diff_reason: None,
            metadata_lines: vec![],
            hunks: vec![],
            lhs_syntax: vec![],
            rhs_syntax: vec![vec![kat_adapter::SyntaxRange {
                start: 0,
                end: 4,
                style: syntax_style((255, 121, 198)),
            }]],
            lhs_document: None,
            rhs_document: None,
        };
        let side = DiffSide {
            line_number: 1,
            text: "true".to_string(),
            diff_ranges: vec![DiffRange {
                start: 0,
                end: 4,
                kind: DiffHighlightKind::Keyword,
            }],
        };

        let spans = styled_content(&file, &side, SideKind::Right, 0);

        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].style.fg, Some(Color::Rgb(255, 121, 198)));
        assert_eq!(
            spans[0].style.bg,
            Some(inline_diff_bg_color(SideKind::Right))
        );
        assert!(!spans[0].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn unchanged_single_column_lines_keep_terminal_background() {
        let file = FileDiff {
            path: "x.rs".to_string(),
            old_path: None,
            status: DiffStatus::Modified,
            line_diff_reason: None,
            metadata_lines: vec![],
            hunks: vec![],
            lhs_syntax: vec![vec![]],
            rhs_syntax: vec![],
            lhs_document: None,
            rhs_document: None,
        };
        let side = DiffSide {
            line_number: 1,
            text: "let value = 1;".to_string(),
            diff_ranges: vec![],
        };

        let lines = render_side(
            &file,
            &side,
            SideLayout {
                total_width: 20,
                gutter_width: 3,
                side_kind: SideKind::Left,
                shared_indent: 0,
                tab_width: DEFAULT_TAB_WIDTH,
                backgrounds: &FileBackgrounds::from_file(&file, DEFAULT_TAB_WIDTH),
            },
        );

        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].style.bg, None);
    }

    #[test]
    fn nested_recipe_background_stays_rectangular_in_diff_rows() {
        let source = "build:\n    echo outer\n        echo nested\n";
        let file = FileDiff {
            path: "justfile".to_string(),
            old_path: None,
            status: DiffStatus::Modified,
            line_diff_reason: None,
            metadata_lines: vec![],
            hunks: vec![],
            lhs_syntax: kat_adapter::syntax_lines_for_test("justfile", source).unwrap(),
            rhs_syntax: vec![],
            lhs_document: kat_adapter::prepare_document("justfile", source).unwrap(),
            rhs_document: None,
        };
        let backgrounds = FileBackgrounds::from_file(&file, DEFAULT_TAB_WIDTH);
        let shaded_columns = |line: &Line<'_>| {
            let mut columns = Vec::new();
            let mut column = 0;
            for span in line.spans.iter().skip(1) {
                for grapheme in span.content.graphemes(true) {
                    let width = UnicodeWidthStr::width(grapheme);
                    if span.style.bg == Some(Color::Rgb(64, 67, 84)) {
                        columns.extend(column..column + width);
                    }
                    column += width;
                }
            }
            columns
        };
        for (line_number, text) in [(2, "    echo outer"), (3, "        echo nested")] {
            let side = DiffSide {
                line_number,
                text: text.to_string(),
                diff_ranges: vec![],
            };
            let lines = render_side(
                &file,
                &side,
                SideLayout {
                    total_width: 30,
                    gutter_width: 3,
                    side_kind: SideKind::Left,
                    shared_indent: 0,
                    tab_width: DEFAULT_TAB_WIDTH,
                    backgrounds: &backgrounds,
                },
            );
            assert_eq!(lines.len(), 1);
            assert_eq!(shaded_columns(&lines[0]), (4..19).collect::<Vec<_>>());
        }
    }

    #[test]
    fn nested_backgrounds_cover_markdown_and_keep_diff_overlays() {
        let source = "```rust\nfn stable() {}\n```\n";
        let document = kat_adapter::prepare_document("README.md", source).unwrap();
        let file = FileDiff {
            path: "README.md".to_string(),
            old_path: None,
            status: DiffStatus::Modified,
            line_diff_reason: None,
            metadata_lines: vec![],
            hunks: vec![],
            lhs_syntax: vec![],
            rhs_syntax: kat_adapter::split_spans(source, document.as_ref()),
            lhs_document: None,
            rhs_document: document,
        };
        let side = DiffSide {
            line_number: 2,
            text: "fn stable() {}".to_string(),
            diff_ranges: vec![DiffRange {
                start: 3,
                end: 9,
                kind: DiffHighlightKind::Normal,
            }],
        };
        let lines = render_side(
            &file,
            &side,
            SideLayout {
                total_width: 24,
                gutter_width: 3,
                side_kind: SideKind::Right,
                shared_indent: 0,
                tab_width: DEFAULT_TAB_WIDTH,
                backgrounds: &FileBackgrounds::from_file(&file, DEFAULT_TAB_WIDTH),
            },
        );
        assert!(
            lines[0].spans[1..].iter().any(|span| {
                span.content.contains("fn") && span.style.bg == Some(Color::Rgb(64, 67, 84))
            }),
            "{lines:?}"
        );
        assert!(lines[0].spans[1..].iter().any(|span| {
            span.content.contains("stable")
                && span.style.bg == Some(inline_diff_bg_color(SideKind::Right))
        }));
    }

    #[test]
    fn changed_line_gutter_uses_diff_background_cue() {
        let file = FileDiff {
            path: "x.rs".to_string(),
            old_path: None,
            status: DiffStatus::Modified,
            line_diff_reason: None,
            metadata_lines: vec![],
            hunks: vec![],
            lhs_syntax: vec![vec![]],
            rhs_syntax: vec![],
            lhs_document: None,
            rhs_document: None,
        };
        let side = DiffSide {
            line_number: 3,
            text: "value".to_string(),
            diff_ranges: vec![DiffRange {
                start: 0,
                end: 5,
                kind: DiffHighlightKind::Normal,
            }],
        };

        let lines = render_side(
            &file,
            &side,
            SideLayout {
                total_width: 20,
                gutter_width: 3,
                side_kind: SideKind::Left,
                shared_indent: 0,
                tab_width: DEFAULT_TAB_WIDTH,
                backgrounds: &FileBackgrounds::from_file(&file, DEFAULT_TAB_WIDTH),
            },
        );

        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines[0].spans[0].style.bg,
            Some(diff_gutter_bg_color(SideKind::Left))
        );
    }

    #[test]
    fn changed_line_body_uses_contiguous_diff_background() {
        let file = FileDiff {
            path: "x.rs".to_string(),
            old_path: None,
            status: DiffStatus::Modified,
            line_diff_reason: None,
            metadata_lines: vec![],
            hunks: vec![],
            lhs_syntax: vec![vec![
                kat_adapter::SyntaxRange {
                    start: 0,
                    end: 4,
                    style: syntax_style((139, 233, 253)),
                },
                kat_adapter::SyntaxRange {
                    start: 6,
                    end: 11,
                    style: syntax_style((241, 250, 140)),
                },
            ]],
            rhs_syntax: vec![],
            lhs_document: None,
            rhs_document: None,
        };
        let side = DiffSide {
            line_number: 7,
            text: "path: value".to_string(),
            diff_ranges: vec![
                DiffRange {
                    start: 0,
                    end: 5,
                    kind: DiffHighlightKind::Normal,
                },
                DiffRange {
                    start: 6,
                    end: 11,
                    kind: DiffHighlightKind::Normal,
                },
            ],
        };

        let lines = render_side(
            &file,
            &side,
            SideLayout {
                total_width: 20,
                gutter_width: 3,
                side_kind: SideKind::Left,
                shared_indent: 0,
                tab_width: DEFAULT_TAB_WIDTH,
                backgrounds: &FileBackgrounds::from_file(&file, DEFAULT_TAB_WIDTH),
            },
        );

        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].spans[1..]
                .iter()
                .filter(|span| span.style.bg == Some(inline_diff_bg_color(SideKind::Left)))
                .count()
                >= 1
        );
    }

    #[test]
    fn whitespace_between_diff_ranges_uses_contiguous_background() {
        let file = FileDiff {
            path: "x.rs".to_string(),
            old_path: None,
            status: DiffStatus::Modified,
            line_diff_reason: None,
            metadata_lines: vec![],
            hunks: vec![],
            lhs_syntax: vec![vec![]],
            rhs_syntax: vec![],
            lhs_document: None,
            rhs_document: None,
        };
        let side = DiffSide {
            line_number: 7,
            text: "foo bar".to_string(),
            diff_ranges: vec![
                DiffRange {
                    start: 0,
                    end: 3,
                    kind: DiffHighlightKind::Normal,
                },
                DiffRange {
                    start: 4,
                    end: 7,
                    kind: DiffHighlightKind::Normal,
                },
            ],
        };

        let spans = styled_content(&file, &side, SideKind::Left, 0);

        assert_eq!(spans.len(), 3);
        assert_eq!(spans[1].content, " ");
        assert_eq!(
            spans[1].style.bg,
            Some(inline_diff_bg_color(SideKind::Left))
        );
    }

    #[test]
    fn changed_line_only_highlights_diff_range_not_whole_line() {
        let file = FileDiff {
            path: "x.rs".to_string(),
            old_path: None,
            status: DiffStatus::Modified,
            line_diff_reason: None,
            metadata_lines: vec![],
            hunks: vec![],
            lhs_syntax: vec![vec![]],
            rhs_syntax: vec![],
            lhs_document: None,
            rhs_document: None,
        };
        let side = DiffSide {
            line_number: 7,
            text: "path: \"x.rs\".to_string(),".to_string(),
            diff_ranges: vec![DiffRange {
                start: 6,
                end: 12,
                kind: DiffHighlightKind::Normal,
            }],
        };

        let lines = render_side(
            &file,
            &side,
            SideLayout {
                total_width: 40,
                gutter_width: 3,
                side_kind: SideKind::Left,
                shared_indent: 0,
                tab_width: DEFAULT_TAB_WIDTH,
                backgrounds: &FileBackgrounds::from_file(&file, DEFAULT_TAB_WIDTH),
            },
        );

        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].style.bg, None);
        assert_eq!(lines[0].spans[1].style.bg, None);
        assert_eq!(
            lines[0].spans[2].style.bg,
            Some(inline_diff_bg_color(SideKind::Left))
        );
        assert_eq!(lines[0].spans[3].style.bg, None);
    }

    #[test]
    fn shared_hunk_indent_trims_common_spaces_but_keeps_relative_layout() {
        let file = FileDiff {
            path: "x.rs".to_string(),
            old_path: None,
            status: DiffStatus::Modified,
            line_diff_reason: None,
            metadata_lines: vec![],
            hunks: vec![],
            lhs_syntax: vec![vec![], vec![]],
            rhs_syntax: vec![vec![], vec![]],
            lhs_document: None,
            rhs_document: None,
        };
        let rows = vec![
            DiffRow::new(
                Some(DiffSide {
                    line_number: 1,
                    text: "      if foo {".to_string(),
                    diff_ranges: vec![],
                }),
                Some(DiffSide {
                    line_number: 1,
                    text: "      if foo {".to_string(),
                    diff_ranges: vec![],
                }),
            ),
            DiffRow::new(
                Some(DiffSide {
                    line_number: 2,
                    text: "        bar();".to_string(),
                    diff_ranges: vec![],
                }),
                Some(DiffSide {
                    line_number: 2,
                    text: "        baz();".to_string(),
                    diff_ranges: vec![DiffRange {
                        start: 8,
                        end: 11,
                        kind: DiffHighlightKind::Normal,
                    }],
                }),
            ),
        ];

        let rhs_indent = shared_hunk_indent(&rows, SideKind::Right);
        assert_eq!(rhs_indent, 6);

        let spans = styled_content(
            &file,
            rows[1].rhs.as_ref().expect("rhs side"),
            SideKind::Right,
            rhs_indent,
        );
        let content = spans
            .iter()
            .map(|span| span.content.as_str())
            .collect::<String>();

        assert_eq!(content, "  baz();");
    }

    #[test]
    fn tab_indentation_is_expanded_for_display() {
        let file = FileDiff {
            path: "x.go".to_string(),
            old_path: None,
            status: DiffStatus::Modified,
            line_diff_reason: None,
            metadata_lines: vec![],
            hunks: vec![],
            lhs_syntax: vec![vec![]],
            rhs_syntax: vec![vec![]],
            lhs_document: None,
            rhs_document: None,
        };
        let side = DiffSide {
            line_number: 1,
            text: "\tcall()".to_string(),
            diff_ranges: vec![],
        };

        let spans = styled_content(&file, &side, SideKind::Right, 0);
        let content = spans
            .iter()
            .map(|span| span.content.as_str())
            .collect::<String>();

        assert_eq!(content, "\tcall()");
        assert_eq!(
            expand_tabs_for_display(&content, 0, DEFAULT_TAB_WIDTH),
            "    call()"
        );
    }

    #[test]
    fn tab_expansion_respects_current_column() {
        assert_eq!(
            expand_tabs_for_display("ab\tc", 0, DEFAULT_TAB_WIDTH),
            "ab  c"
        );
        assert_eq!(expand_tabs_for_display("\tc", 2, DEFAULT_TAB_WIDTH), "  c");
    }

    #[test]
    fn custom_tab_width_is_applied_to_rendering() {
        assert_eq!(expand_tabs_for_display("\tc", 0, 8), "        c");
    }

    #[test]
    fn wrapping_keeps_joined_emoji_in_one_display_cell_group() {
        let mut spans = vec![StyledSpan {
            content: "👩‍💻x".to_string(),
            style: Style::default(),
        }];
        let rows = wrap_segments(&mut spans, 2, DEFAULT_TAB_WIDTH, 0);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0].content, "👩‍💻");
        assert_eq!(rows[1][0].content, "x");
    }

    #[test]
    fn wrapping_moves_wide_grapheme_to_next_row_when_one_cell_remains() {
        let mut spans = vec![StyledSpan {
            content: "a中".to_string(),
            style: Style::default(),
        }];
        let rows = wrap_segments(&mut spans, 2, DEFAULT_TAB_WIDTH, 0);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0].content, "a");
        assert_eq!(rows[1][0].content, "中");
    }

    #[test]
    fn replace_files_preserves_selection_across_rename() {
        let mut app = AppState::new(vec![flat_file("old.rs"), flat_file("other.rs")], 120);
        app.committed_file = 0;

        app.replace_files(vec![
            FileDiff {
                path: "new.rs".to_string(),
                old_path: Some("old.rs".to_string()),
                status: DiffStatus::Renamed,
                line_diff_reason: None,
                metadata_lines: vec![],
                hunks: vec![],
                lhs_syntax: vec![],
                rhs_syntax: vec![],
                lhs_document: None,
                rhs_document: None,
            },
            flat_file("other.rs"),
        ]);

        assert_eq!(app.committed_file, 0);
        assert_eq!(app.current_file().map(|f| f.path.as_str()), Some("new.rs"));
    }

    #[test]
    fn sidebar_uses_flat_abbreviated_paths() {
        let app = AppState::new(
            vec![
                flat_file("src/foo/janus.txt"),
                flat_file("src/fizz/janus.txt"),
                flat_file("scripts/foo/janus.txt"),
            ],
            120,
        );
        let entries = app.visible_sidebar_entries();

        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].label(), "[M] sr/fo/janus.txt");
        assert_eq!(entries[1].label(), "[M] sr/fi/janus.txt");
        assert_eq!(entries[2].label(), "[M] sc/f/janus.txt");
    }

    #[test]
    fn sidebar_width_tracks_longest_abbreviated_entry() {
        let app = AppState::new(
            vec![
                flat_file("src/foo/janus.txt"),
                flat_file("scripts/really-long-name/example.rs"),
            ],
            120,
        );

        let longest = app
            .visible_sidebar_entries()
            .into_iter()
            .map(|entry| UnicodeWidthStr::width(entry.label().as_str()) as u16)
            .max()
            .unwrap();

        assert_eq!(app.sidebar_width, longest + 2);
    }

    #[test]
    fn app_starts_with_sidebar_focused() {
        let app = AppState::new(vec![flat_file("src/main.rs")], 120);
        assert_eq!(app.route, Route::FileList);
    }

    #[test]
    fn horizontal_cursor_moves_switch_focus_between_panes() {
        let mut app = AppState::new(vec![flat_file("src/main.rs")], 120);

        app.move_cursor_right();
        assert_eq!(app.route, Route::Diff);

        app.move_cursor_left();
        assert_eq!(app.route, Route::FileList);
    }

    #[test]
    fn vertical_cursor_moves_line_diff_and_step_sidebar() {
        let mut app = AppState::new(vec![big_file(), flat_file("other.rs")], 120);
        app.update_ui_layout(UiLayout {
            sidebar_inner: Rect::new(0, 1, 32, 12),
            diff_area: Rect::new(33, 1, 87, 12),
            diff_inner: Rect::new(33, 1, 87, 12),
        });

        assert_eq!(app.committed_file, 0);
        app.move_cursor_down();
        assert_eq!(app.committed_file, 1);

        app.move_cursor_up();
        assert_eq!(app.committed_file, 0);

        app.route = Route::Diff;
        app.move_cursor_down();
        assert_eq!(app.scroll, 1);

        let scrolled = app.scroll;
        app.move_cursor_up();
        assert_eq!(app.scroll + 1, scrolled);
    }

    #[test]
    fn sidebar_search_filters_list_and_expands_matching_paths() {
        let mut app = AppState::new(
            vec![
                flat_file("src/main.rs"),
                flat_file("src/model.rs"),
                flat_file("docs/readme.md"),
            ],
            120,
        );
        app.committed_file = 2;

        app.start_sidebar_search();
        app.sidebar_search_push('m');
        app.sidebar_search_push('r');

        let entries = app.visible_sidebar_entries();
        assert_eq!(
            app.current_file().map(|file| file.path.as_str()),
            Some("src/main.rs")
        );
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.original_path.as_str())
                .collect::<Vec<_>>(),
            vec!["src/main.rs", "src/model.rs"]
        );
        assert_eq!(entries[0].display_path, "src/main.rs");
        assert!(!entries[0].matched_positions.is_empty());
        assert_eq!(entries.iter().filter(|entry| entry.is_selected).count(), 1);
        assert_eq!(
            entries.iter().filter(|entry| entry.is_active_file).count(),
            1
        );
    }

    #[test]
    fn sidebar_search_returns_all_matches_in_rank_order() {
        let mut app = AppState::new(
            vec![
                flat_file("src/main.rs"),
                flat_file("src/model.rs"),
                flat_file("src/mode.rs"),
                flat_file("src/module.rs"),
                flat_file("src/mocks.rs"),
                flat_file("src/monitor.rs"),
            ],
            120,
        );

        app.start_sidebar_search();
        app.sidebar_search_push('m');
        app.sidebar_search_push('o');

        let matches = app.visible_sidebar_entries();
        assert_eq!(matches.len(), 5);
        assert!(
            matches
                .iter()
                .any(|entry| entry.original_path == "src/model.rs")
        );
    }

    #[test]
    fn sidebar_search_select_confirms_match() {
        let mut app = AppState::new(
            vec![
                flat_file("src/main.rs"),
                flat_file("src/model.rs"),
                flat_file("docs/readme.md"),
            ],
            120,
        );

        app.start_sidebar_search();
        app.sidebar_search_push('m');
        app.sidebar_search_push('o');
        app.sidebar_search_push('d');
        app.select_current();

        assert!(!app.is_sidebar_search_active());
        assert_eq!(app.route, Route::Diff);
        assert_eq!(
            app.current_file().map(|file| file.path.as_str()),
            Some("src/model.rs")
        );
    }

    #[test]
    fn escape_from_search_restores_original_file_and_repeat_stays_on_sidebar() {
        let mut app = AppState::new(
            vec![flat_file("src/main.rs"), flat_file("src/model.rs")],
            120,
        );

        app.start_sidebar_search();
        app.sidebar_search_push('m');
        app.sidebar_search_push('o');
        app.sidebar_search_push('d');
        assert_eq!(
            app.current_file().map(|file| file.path.as_str()),
            Some("src/model.rs")
        );

        assert_eq!(
            app.visible_sidebar_entries()
                .iter()
                .find(|entry| entry.is_selected)
                .map(|entry| entry.original_path.as_str()),
            Some("src/model.rs")
        );
        app.escape();
        assert_eq!(app.route, Route::FileList);
        assert!(!app.is_sidebar_search_active());
        assert_eq!(
            app.current_file().map(|file| file.path.as_str()),
            Some("src/main.rs")
        );

        app.escape();
        assert_eq!(app.route, Route::FileList);
    }

    #[test]
    fn sidebar_search_selection_previews_file_before_confirm() {
        let mut app = AppState::new(
            vec![
                flat_file("src/main.rs"),
                flat_file("src/model.rs"),
                flat_file("src/module.rs"),
            ],
            120,
        );

        app.start_sidebar_search();
        app.sidebar_search_push('m');
        app.sidebar_search_push('o');
        app.sidebar_search_push('d');
        assert_eq!(
            app.current_file().map(|file| file.path.as_str()),
            Some("src/model.rs")
        );
    }

    #[test]
    fn compact_layout_starts_on_sidebar_and_enter_opens_diff() {
        let mut app = AppState::new(
            vec![flat_file("src/main.rs"), flat_file("src/model.rs")],
            80,
        );

        assert!(app.is_compact_layout());
        assert_eq!(app.route, Route::FileList);

        app.select_current();
        assert_eq!(app.route, Route::Diff);
    }

    #[test]
    fn compact_layout_escape_from_diff_returns_to_sidebar() {
        let mut app = AppState::new(
            vec![flat_file("src/main.rs"), flat_file("src/model.rs")],
            80,
        );

        app.select_current();
        assert_eq!(app.route, Route::Diff);

        app.escape();
        assert_eq!(app.route, Route::FileList);
    }

    #[test]
    fn resize_to_compact_layout_enables_single_pane_navigation() {
        let mut app = AppState::new(
            vec![flat_file("src/main.rs"), flat_file("src/model.rs")],
            120,
        );

        assert!(!app.is_compact_layout());
        app.sync_terminal_width(80);
        assert!(app.is_compact_layout());

        app.select_current();
        assert_eq!(app.route, Route::Diff);
        app.escape();
        assert_eq!(app.route, Route::FileList);
    }

    #[test]
    fn compact_layout_q_from_diff_returns_to_sidebar_instead_of_quitting() {
        let mut app = AppState::new(
            vec![flat_file("src/main.rs"), flat_file("src/model.rs")],
            80,
        );

        app.select_current();
        assert_eq!(app.route, Route::Diff);
        assert!(!app.should_quit_on_q());
        assert_eq!(app.route, Route::FileList);
        assert!(app.should_quit_on_q());
    }

    #[test]
    fn jump_next_hunk_can_anchor_last_hunk_at_top() {
        let file = FileDiff {
            path: "x.rs".to_string(),
            old_path: None,
            status: DiffStatus::Modified,
            line_diff_reason: None,
            metadata_lines: vec![],
            hunks: vec![
                DiffHunk {
                    title: Some("x.rs --- 1/2 --- Rust".to_string()),
                    rows: vec![DiffRow::new(
                        Some(DiffSide {
                            line_number: 1,
                            text: "let a = 1;".to_string(),
                            diff_ranges: vec![],
                        }),
                        Some(DiffSide {
                            line_number: 1,
                            text: "let a = 2;".to_string(),
                            diff_ranges: vec![],
                        }),
                    )],
                },
                DiffHunk {
                    title: Some("x.rs --- 2/2 --- Rust".to_string()),
                    rows: vec![DiffRow::new(
                        Some(DiffSide {
                            line_number: 8,
                            text: "before();".to_string(),
                            diff_ranges: vec![],
                        }),
                        Some(DiffSide {
                            line_number: 8,
                            text: "after();".to_string(),
                            diff_ranges: vec![],
                        }),
                    )],
                },
            ],
            lhs_syntax: vec![vec![]; 8],
            rhs_syntax: vec![vec![]; 8],
            lhs_document: None,
            rhs_document: None,
        };

        let mut app = AppState::new(vec![file], 120);
        app.update_ui_layout(UiLayout {
            sidebar_inner: Rect::new(0, 1, 20, 10),
            diff_area: Rect::new(21, 1, 99, 10),
            diff_inner: Rect::new(21, 1, 99, 10),
        });
        app.route = Route::Diff;

        app.jump_next_hunk();
        assert_eq!(app.scroll, 3);
    }

    #[test]
    fn single_column_render_keeps_blank_lines() {
        let file = FileDiff {
            path: "blank.txt".to_string(),
            old_path: None,
            status: DiffStatus::Created,
            line_diff_reason: None,
            metadata_lines: vec![],
            hunks: vec![DiffHunk {
                title: Some("1/1 --- Text".to_string()),
                rows: vec![
                    DiffRow::new(
                        None,
                        Some(DiffSide {
                            line_number: 1,
                            text: "first".to_string(),
                            diff_ranges: vec![],
                        }),
                    ),
                    DiffRow::new(
                        None,
                        Some(DiffSide {
                            line_number: 2,
                            text: "".to_string(),
                            diff_ranges: vec![],
                        }),
                    ),
                    DiffRow::new(
                        None,
                        Some(DiffSide {
                            line_number: 3,
                            text: "third".to_string(),
                            diff_ranges: vec![],
                        }),
                    ),
                ],
            }],
            lhs_syntax: vec![],
            rhs_syntax: vec![vec![], vec![], vec![]],
            lhs_document: None,
            rhs_document: None,
        };

        let layout = DiffLayout::build(&file, 40);
        let rendered = layout.rendered_text();
        assert!(
            rendered
                .lines
                .iter()
                .any(|line| { line.spans.iter().any(|span| span.content.contains("2 ")) })
        );
    }

    fn sample_file() -> FileDiff {
        FileDiff {
            path: "new.rs".to_string(),
            old_path: Some("old.rs".to_string()),
            status: DiffStatus::Renamed,
            line_diff_reason: None,
            metadata_lines: vec![],
            hunks: vec![],
            lhs_syntax: vec![],
            rhs_syntax: vec![],
            lhs_document: None,
            rhs_document: None,
        }
    }

    fn flat_file(path: &str) -> FileDiff {
        FileDiff {
            path: path.to_string(),
            old_path: None,
            status: DiffStatus::Modified,
            line_diff_reason: None,
            metadata_lines: vec![],
            hunks: vec![],
            lhs_syntax: vec![],
            rhs_syntax: vec![],
            lhs_document: None,
            rhs_document: None,
        }
    }

    fn big_file() -> FileDiff {
        FileDiff {
            path: "big.rs".to_string(),
            old_path: None,
            status: DiffStatus::Modified,
            line_diff_reason: None,
            metadata_lines: vec![],
            hunks: vec![DiffHunk {
                title: None,
                rows: (0..100)
                    .map(|idx| {
                        DiffRow::new(
                            Some(DiffSide {
                                line_number: idx + 1,
                                text: format!("before {idx}"),
                                diff_ranges: vec![],
                            }),
                            Some(DiffSide {
                                line_number: idx + 1,
                                text: format!("after {idx}"),
                                diff_ranges: vec![],
                            }),
                        )
                    })
                    .collect(),
            }],
            lhs_syntax: vec![vec![]; 100],
            rhs_syntax: vec![vec![]; 100],
            lhs_document: None,
            rhs_document: None,
        }
    }
}
