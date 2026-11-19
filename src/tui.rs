// shows the fetched pull requests as a full screen, fuzzy searchable table.

use std::cmp::Reverse;
use std::io;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Cell, HighlightSpacing, Paragraph, Row, Scrollbar, ScrollbarOrientation, ScrollbarState,
    Table, TableState,
};
use ratatui::{DefaultTerminal, Frame};
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::pulls::PullRequest;

/// columns the table renders between adjacent columns.
const SPACING: usize = 2;
/// `SPACING` as the literal spaces joined between columns in a rendered row.
const SPACES: &str = "  ";
/// width reserved in front of every row for the selection marker.
const MARKER_WIDTH: usize = 2;
/// narrowest content window that still reads as a table before the browser
/// gives up on the terminal width.
const MIN_BUDGET: usize = 10;
/// display columns a single pan keypress scrolls the table by.
const PAN_STEP: usize = 8;
/// braille frames the refresh indicator cycles through.
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
/// how often the browser wakes up while refreshing to animate the spinner and
/// poll for the finished fetch.
const REFRESH_TICK: Duration = Duration::from_millis(120);

/// re-fetches the current open pull requests for a scope, used by the `R`
/// refresh shortcut.
pub type RefreshFn = Arc<dyn Fn() -> Result<Vec<PullRequest>> + Send + Sync>;

/// opens a pull request url in the user's browser, used by the `enter` key.
pub type OpenFn = Arc<dyn Fn(&str) -> Result<()>>;

/// shows the fetched pull requests in a full screen fuzzy searchable table.
///
/// `refresh` re-fetches the list whenever the user presses `R`, and `open`
/// launches a pull request in the browser whenever they press `enter`; both
/// keep the browser running. it quits only on `esc` or `ctrl+c`, returning the
/// urls the user opened during the session.
pub fn select_pull_request(
    scope: &str,
    pull_requests: Vec<PullRequest>,
    refresh: RefreshFn,
    open: OpenFn,
) -> Result<Vec<String>> {
    let mut app = App::new(scope, pull_requests, refresh, open);
    ratatui::run(|terminal| app.run(terminal)).context("failed to run the pull request browser")
}

/// what the browser should do next after a keypress.
#[derive(Debug, PartialEq, Eq)]
enum Action {
    Continue,
    Cancel,
}

/// a transient message shown at the end of the help line.
#[derive(Debug, PartialEq, Eq)]
enum Notice {
    /// a pull request was opened in the browser, as `owner/repository#number`.
    Opened(String),
    /// the last refresh or open failed.
    Error(String),
}

struct App {
    scope: String,
    pull_requests: Vec<PullRequest>,
    input: Input,
    matcher: Matcher,
    /// indices into `pull_requests` matching the current query, best first.
    matched: Vec<usize>,
    state: TableState,
    /// rows the table could show during the last draw, used for paging.
    viewport: usize,
    /// display columns of the table hidden on the left by horizontal panning.
    offset: usize,
    /// the largest `offset` that still reveals content, from the last draw.
    max_offset: usize,
    number_width: usize,
    repository_width: usize,
    author_width: usize,
    /// repeats the original fetch when the user presses `R`.
    fetch: RefreshFn,
    /// launches the highlighted pull request when the user presses `enter`.
    open: OpenFn,
    /// the in-flight re-fetch, until its result lands.
    refresh: Option<Receiver<Result<Vec<PullRequest>>>>,
    /// frame counter for the refresh spinner.
    spinner: usize,
    /// a transient message about the last refresh or open, shown in the help
    /// line.
    notice: Option<Notice>,
    /// urls opened during the session, handed back when the browser exits.
    opened: Vec<String>,
}

impl App {
    fn new(scope: &str, pull_requests: Vec<PullRequest>, refresh: RefreshFn, open: OpenFn) -> Self {
        let matched = (0..pull_requests.len()).collect::<Vec<_>>();
        let selected = (!matched.is_empty()).then_some(0);

        let mut app = Self {
            scope: scope.to_string(),
            pull_requests,
            input: Input::default(),
            matcher: Matcher::new(Config::DEFAULT),
            matched,
            state: TableState::default().with_selected(selected),
            viewport: 0,
            offset: 0,
            max_offset: 0,
            number_width: 0,
            repository_width: 0,
            author_width: 0,
            fetch: refresh,
            open,
            refresh: None,
            spinner: 0,
            notice: None,
            opened: Vec::new(),
        };
        app.compute_widths();
        app
    }

    /// sizes the fixed columns to their content, never narrower than their
    /// headers.
    fn compute_widths(&mut self) {
        // columns are sized by their content and the whole table pans, so
        // nothing is ever truncated except the header minimums.
        self.number_width = self
            .pull_requests
            .iter()
            .map(|pull_request| pull_request.number.to_string().len())
            .max()
            .unwrap_or(1);
        self.repository_width = self
            .pull_requests
            .iter()
            .map(|pull_request| UnicodeWidthStr::width(pull_request.repository.as_str()))
            .max()
            .unwrap_or(0)
            .max("REPOSITORY".len());
        self.author_width = self
            .pull_requests
            .iter()
            .map(|pull_request| UnicodeWidthStr::width(pull_request.author.as_str()))
            .max()
            .unwrap_or(0)
            .max("AUTHOR".len());
    }

    fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<Vec<String>> {
        loop {
            terminal.draw(|frame| self.draw(frame))?;

            let event = if self.refresh.is_some() {
                // while a refresh is in flight, wake up periodically so the
                // spinner animates and the result lands promptly.
                if event::poll(REFRESH_TICK)? {
                    event::read()?
                } else {
                    self.spinner += 1;
                    self.check_refresh();
                    continue;
                }
            } else {
                event::read()?
            };

            match self.handle_event(&event) {
                Action::Continue => {}
                Action::Cancel => return Ok(self.opened.clone()),
            }
        }
    }

    fn handle_event(&mut self, event: &Event) -> Action {
        let Event::Key(key) = event else {
            // resize events are handled by drawing again on the next loop turn.
            return Action::Continue;
        };

        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return Action::Continue;
        }

        match key.code {
            KeyCode::Esc => Action::Cancel,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Action::Cancel,
            KeyCode::Enter => {
                self.open_selected();
                Action::Continue
            }
            KeyCode::Down => {
                self.step(1);
                Action::Continue
            }
            KeyCode::Up => {
                self.step(-1);
                Action::Continue
            }
            KeyCode::PageDown => {
                self.page(self.viewport.max(1) as isize);
                Action::Continue
            }
            KeyCode::PageUp => {
                self.page(-(self.viewport.max(1) as isize));
                Action::Continue
            }
            KeyCode::Left if pans(key.modifiers) => {
                self.pan(-(PAN_STEP as isize));
                Action::Continue
            }
            KeyCode::Right if pans(key.modifiers) => {
                self.pan(PAN_STEP as isize);
                Action::Continue
            }
            // macOS terminals often send option+arrow as `alt` + `b`/`f`.
            KeyCode::Char('b') if pans(key.modifiers) => {
                self.pan(-(PAN_STEP as isize));
                Action::Continue
            }
            KeyCode::Char('f') if pans(key.modifiers) => {
                self.pan(PAN_STEP as isize);
                Action::Continue
            }
            KeyCode::Home if pans(key.modifiers) => {
                self.offset = 0;
                Action::Continue
            }
            KeyCode::End if pans(key.modifiers) => {
                self.offset = self.max_offset;
                Action::Continue
            }
            // shift+r re-fetches; lowercase r stays a query character.
            KeyCode::Char('R') => {
                self.start_refresh();
                Action::Continue
            }
            _ => {
                // typing edits the query; cursor-only movements do not requery.
                if let Some(changed) = self.input.handle_event(event)
                    && changed.value
                {
                    self.requery();
                }
                Action::Continue
            }
        }
    }

    /// the index into `pull_requests` of the highlighted row, if any.
    fn selected_index(&self) -> Option<usize> {
        self.state
            .selected()
            .and_then(|position| self.matched.get(position))
            .copied()
    }

    /// moves the highlight by `delta` rows, wrapping around the ends so the
    /// arrows cycle the list like a loop.
    fn step(&mut self, delta: isize) {
        if self.matched.is_empty() {
            return;
        }

        let length = self.matched.len() as isize;
        let current = self.state.selected().unwrap_or(0) as isize;
        let next = (current + delta).rem_euclid(length);
        self.state.select(Some(next as usize));
    }

    /// moves the highlight by `delta` rows without wrapping around, for
    /// paging.
    fn page(&mut self, delta: isize) {
        if self.matched.is_empty() {
            return;
        }

        let last = (self.matched.len() - 1) as isize;
        let current = self.state.selected().unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, last) as usize;
        self.state.select(Some(next));
    }

    /// scrolls the whole table `delta` display columns sideways, clamped to
    /// the widest row on screen.
    fn pan(&mut self, delta: isize) {
        let next = (self.offset as isize + delta).clamp(0, self.max_offset as isize);
        self.offset = next as usize;
    }

    /// opens the highlighted pull request in the browser and keeps browsing;
    /// the footer reports the outcome.
    fn open_selected(&mut self) {
        let Some(index) = self.selected_index() else {
            return;
        };

        let pull_request = &self.pull_requests[index];
        let url = pull_request.url();
        let reference = format!(
            "{}/{}#{}",
            pull_request.owner, pull_request.repository, pull_request.number
        );

        match (self.open)(&url) {
            Ok(()) => {
                self.opened.push(url);
                self.notice = Some(Notice::Opened(reference));
            }
            Err(error) => {
                self.notice = Some(Notice::Error(format!("failed to open: {error}")));
            }
        }
    }

    /// re-fetches the open pull requests on a background thread so browsing
    /// stays responsive while GitHub is queried.
    fn start_refresh(&mut self) {
        if self.refresh.is_some() {
            return;
        }

        let (sender, receiver) = mpsc::channel();
        let fetch = Arc::clone(&self.fetch);
        thread::spawn(move || {
            // a closed channel just means the browser already exited.
            let _ = sender.send(fetch());
        });

        self.refresh = Some(receiver);
        self.spinner = 0;
        self.notice = None;
    }

    /// picks up the result of an in-flight refresh that has landed.
    fn check_refresh(&mut self) {
        let received = self.refresh.as_ref().map(Receiver::try_recv);

        match received {
            Some(Ok(Ok(pull_requests))) => {
                self.refresh = None;
                self.apply_refresh(pull_requests);
            }
            Some(Ok(Err(error))) => {
                self.refresh = None;
                self.notice = Some(Notice::Error(format!("refresh failed: {error}")));
            }
            Some(Err(TryRecvError::Disconnected)) => {
                self.refresh = None;
                self.notice = Some(Notice::Error("refresh failed".to_string()));
            }
            Some(Err(TryRecvError::Empty)) | None => {}
        }
    }

    /// swaps in freshly fetched pull requests, keeping the highlighted one
    /// selected when it is still open.
    fn apply_refresh(&mut self, pull_requests: Vec<PullRequest>) {
        let previous = self.highlighted_marker();

        self.pull_requests = pull_requests;
        self.compute_widths();
        self.requery();

        if let Some((number, repository)) = previous
            && let Some(position) = self.matched.iter().position(|index| {
                let pull_request = &self.pull_requests[*index];
                pull_request.number == number && pull_request.repository == repository
            })
        {
            self.state.select(Some(position));
        }

        self.notice = None;
    }

    /// `(number, repository)` of the highlighted pull request, used to find it
    /// again after a refresh.
    fn highlighted_marker(&self) -> Option<(u64, String)> {
        self.selected_index().map(|index| {
            let pull_request = &self.pull_requests[index];
            (pull_request.number, pull_request.repository.clone())
        })
    }

    /// re-ranks the pull requests against the query in the search box.
    fn requery(&mut self) {
        let pattern = Pattern::parse(
            self.input.value(),
            CaseMatching::Smart,
            Normalization::Smart,
        );
        let pull_requests = &self.pull_requests;
        let matcher = &mut self.matcher;
        let mut buffer = Vec::new();

        let mut scored: Vec<(usize, u32)> = if pattern.atoms.is_empty() {
            (0..pull_requests.len()).map(|index| (index, 0)).collect()
        } else {
            pull_requests
                .iter()
                .enumerate()
                .filter_map(|(index, pull_request)| {
                    let label = pull_request.label();
                    let haystack = Utf32Str::new(&label, &mut buffer);
                    pattern.score(haystack, matcher).map(|score| (index, score))
                })
                .collect()
        };

        // the sort is stable, so equal scores keep the fetched recency order.
        scored.sort_by_key(|(_, score)| Reverse(*score));

        self.matched = scored.into_iter().map(|(index, _)| index).collect();
        self.state.select((!self.matched.is_empty()).then_some(0));
        self.offset = 0;
    }

    fn draw(&mut self, frame: &mut Frame) {
        let [search_area, table_area, help_area] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .areas(frame.area());

        self.draw_search(frame, search_area);
        self.draw_table(frame, table_area);
        self.draw_help(frame, help_area);
    }

    fn draw_search(&mut self, frame: &mut Frame, area: Rect) {
        let title = format!(
            " zevl · {} · {} open pull requests ",
            self.scope,
            self.pull_requests.len()
        );
        let block = Block::bordered().title(title);
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if inner.width == 0 {
            return;
        }

        let width = inner.width.saturating_sub(1) as usize;
        let scroll = self.input.visual_scroll(width);

        let line = if self.input.value().is_empty() {
            Line::from("type to filter").dim()
        } else {
            Line::from(self.input.value())
        };
        frame.render_widget(Paragraph::new(line).scroll((0, scroll as u16)), inner);

        let cursor = self.input.visual_cursor().max(scroll) - scroll;
        frame.set_cursor_position((inner.x + cursor as u16, inner.y));
    }

    fn draw_table(&mut self, frame: &mut Frame, area: Rect) {
        let block = Block::bordered();
        let inner = block.inner(area);
        self.viewport = inner.height as usize;

        let scrollbar = self.matched.len() > self.viewport;
        let budget = (inner.width as usize).saturating_sub(MARKER_WIDTH + usize::from(scrollbar));

        if budget < MIN_BUDGET || self.matched.is_empty() {
            self.offset = 0;
            self.max_offset = 0;
            let message = if budget < MIN_BUDGET {
                "too narrow"
            } else if self.input.value().is_empty() {
                "no open pull requests"
            } else {
                "no pull requests match your search"
            };
            frame.render_widget(block.title(self.table_title()), area);
            frame.render_widget(Paragraph::new(message).centered().dim(), inner);
            return;
        }

        // the whole table pans: the viewport slides across the widest row and
        // content is only ever cut at the terminal edges, never ellipsized.
        let widest_title = self
            .matched
            .iter()
            .map(|index| UnicodeWidthStr::width(self.pull_requests[*index].title.as_str()))
            .max()
            .unwrap_or(0);
        let widest = widest_title.max(UnicodeWidthStr::width("TITLE"));
        self.max_offset = (self.fixed_width() + widest).saturating_sub(budget);
        self.offset = self.offset.min(self.max_offset);

        let header = Row::new([Cell::from(slice(&self.table_header(), self.offset, budget))])
            .style(Style::new().bold());

        let rows = self
            .matched
            .iter()
            .map(|index| {
                Row::new([Cell::from(slice(
                    &self.row_line(*index),
                    self.offset,
                    budget,
                ))])
            })
            .collect::<Vec<_>>();

        let table = Table::new(rows, [Constraint::Fill(1)])
            .header(header)
            .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD))
            .highlight_symbol("▶ ")
            .highlight_spacing(HighlightSpacing::Always)
            .block(block.title(self.table_title()));

        frame.render_stateful_widget(table, area, &mut self.state);

        if scrollbar {
            let scrollbar_area = Rect {
                x: inner.x + inner.width - 1,
                y: inner.y,
                width: 1,
                height: inner.height,
            };
            let mut scrollbar_state = ScrollbarState::new(self.matched.len())
                .position(self.state.offset())
                .viewport_content_length(self.viewport);
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(None)
                    .end_symbol(None),
                scrollbar_area,
                &mut scrollbar_state,
            );
        }
    }

    fn draw_help(&self, frame: &mut Frame, area: Rect) {
        let mut spans = vec![Span::raw("  ")];

        // the notice leads so it stays visible on narrow terminals.
        if let Some(notice) = &self.notice {
            let (message, style) = match notice {
                Notice::Opened(reference) => (format!("opened {reference}"), Style::new().green()),
                Notice::Error(message) => (message.clone(), Style::new().red()),
            };
            spans.push(Span::styled(message, style));
            spans.push(" · ".dim());
        }

        spans.extend([
            "↑/↓".bold(),
            " move · ".dim(),
            "pgup/pgdn".bold(),
            " jump · ".dim(),
        ]);
        if self.max_offset > 0 {
            spans.push("shift+←/→".bold());
            spans.push(" pan · ".dim());
        }
        spans.extend([
            "R".bold(),
            " refresh · ".dim(),
            "enter".bold(),
            " open · ".dim(),
            "esc".bold(),
            " quit".dim(),
        ]);
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    /// the table block title, with a spinner while refreshing and arrows that
    /// light up while rows can pan.
    fn table_title(&self) -> Line<'static> {
        let mut label = if self.input.value().is_empty() {
            format!(" {} open pull requests ", self.matched.len())
        } else {
            format!(
                " {} of {} pull requests ",
                self.matched.len(),
                self.pull_requests.len()
            )
        };

        if self.refresh.is_some() {
            let frame = SPINNER[self.spinner % SPINNER.len()];
            label = format!("{frame} {label}");
        }

        if self.max_offset == 0 {
            return Line::from(label);
        }

        let dim = Style::new().add_modifier(Modifier::DIM);
        let left = if self.offset > 0 {
            Span::raw("◂")
        } else {
            Span::styled("◂", dim)
        };
        let right = if self.offset < self.max_offset {
            Span::raw("▸")
        } else {
            Span::styled("▸", dim)
        };

        Line::from(vec![left, Span::raw(label), right])
    }

    /// the header row, aligned with the data columns and pannable with them.
    fn table_header(&self) -> String {
        format!(
            "{:>number$}{SPACES}{}{SPACES}{}{SPACES}TITLE",
            "#",
            pad("REPOSITORY", self.repository_width),
            pad("AUTHOR", self.author_width),
            number = self.number_width,
        )
    }

    /// the full, unpanned row for `index`: fixed width columns joined by
    /// `SPACES`, the title last and unbounded.
    fn row_line(&self, index: usize) -> String {
        let pull_request = &self.pull_requests[index];
        format!(
            "{:>number$}{SPACES}{}{SPACES}{}{SPACES}{}",
            pull_request.number,
            pad(&pull_request.repository, self.repository_width),
            pad(&pull_request.author, self.author_width),
            pull_request.title,
            number = self.number_width,
        )
    }

    /// columns before `TITLE`, including the spacing between them.
    fn fixed_width(&self) -> usize {
        self.number_width + self.repository_width + self.author_width + SPACING * 3
    }
}

/// whether the key modifiers mark a horizontal pan of the table rather than a
/// query edit: `shift` everywhere, plus the macOS option+arrow aliases.
fn pans(modifiers: KeyModifiers) -> bool {
    modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
}

/// left-aligns `text` inside `width` display columns.
fn pad(text: &str, width: usize) -> String {
    let used = UnicodeWidthStr::width(text);
    let mut padded = text.to_string();
    if used < width {
        padded.push_str(&" ".repeat(width - used));
    }
    padded
}

/// returns the `width` display columns of `text` starting at `offset`, cutting
/// content at both edges the way a spreadsheet viewport does.
///
/// graphemes are never split, so wide characters and emoji stay intact; a
/// grapheme straddling the left edge slides into view whole.
fn slice(text: &str, offset: usize, width: usize) -> String {
    if width == 0 {
        return String::new();
    }

    let mut result = String::new();
    let mut skipped = 0;
    let mut used = 0;
    let mut started = false;

    for grapheme in text.graphemes(true) {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if !started && skipped + grapheme_width <= offset {
            skipped += grapheme_width;
            continue;
        }
        started = true;
        if used + grapheme_width > width {
            break;
        }
        result.push_str(grapheme);
        used += grapheme_width;
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crossterm::event::KeyEvent;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn pull_request(number: u64, repository: &str, title: &str, author: &str) -> PullRequest {
        PullRequest {
            number,
            owner: "introbotai".to_string(),
            repository: repository.to_string(),
            title: title.to_string(),
            author: author.to_string(),
        }
    }

    fn pull_requests() -> Vec<PullRequest> {
        vec![
            pull_request(
                416,
                "cluster-admin",
                "show india company ranking completion",
                "vegito5ama",
            ),
            pull_request(
                30,
                "api-exhibitors",
                "add gstin to form exports",
                "VallabhSangvikar",
            ),
            pull_request(14, "zyntra", "forward webhook data", "tanisheesh"),
        ]
    }

    fn app(pull_requests: &[PullRequest]) -> App {
        App::new(
            "introbotai",
            pull_requests.to_vec(),
            idle_fetch(),
            silent_open(),
        )
    }

    /// a fetcher for tests that never press `R`; refresh tests inject their own.
    fn idle_fetch() -> RefreshFn {
        Arc::new(|| Ok(Vec::new()))
    }

    /// an opener for tests that never press `enter`; open tests inject their own.
    fn silent_open() -> OpenFn {
        Arc::new(|_| Ok(()))
    }

    fn render(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal.draw(|frame| app.draw(frame)).expect("draw");

        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn type_query(app: &mut App, query: &str) {
        app.input = query.into();
        app.requery();
    }

    fn press(app: &mut App, code: KeyCode) -> Action {
        app.handle_event(&Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn press_with(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> Action {
        app.handle_event(&Event::Key(KeyEvent::new(code, modifiers)))
    }

    #[test]
    fn empty_query_keeps_the_fetched_recency_order() {
        let pull_requests = pull_requests();
        let app = app(&pull_requests);

        assert_eq!(app.matched, vec![0, 1, 2]);
    }

    #[test]
    fn typing_filters_and_ranks_by_relevance() {
        let pull_requests = pull_requests();
        let mut app = app(&pull_requests);

        type_query(&mut app, "webhook");
        assert_eq!(app.matched, vec![2]);

        type_query(&mut app, "cluster");
        assert_eq!(app.matched, vec![0]);

        type_query(&mut app, "zzz");
        assert!(app.matched.is_empty());

        type_query(&mut app, "");
        assert_eq!(app.matched, vec![0, 1, 2]);
    }

    #[test]
    fn fuzzy_query_matches_subsequences() {
        let pull_requests = pull_requests();
        let mut app = app(&pull_requests);

        type_query(&mut app, "clstr");
        assert_eq!(app.matched, vec![0]);
    }

    #[test]
    fn requery_resets_the_selection_to_the_first_match() {
        let pull_requests = pull_requests();
        let mut app = app(&pull_requests);

        press(&mut app, KeyCode::Down);
        assert_eq!(app.state.selected(), Some(1));

        type_query(&mut app, "a");
        assert_eq!(app.state.selected(), Some(0));
    }

    #[test]
    fn enter_opens_the_selected_pull_request_and_keeps_browsing() {
        let pull_requests = pull_requests();
        let mut app = app(&pull_requests);

        let opened = Arc::new(Mutex::new(Vec::new()));
        app.open = {
            let opened = Arc::clone(&opened);
            Arc::new(move |url: &str| {
                opened.lock().unwrap().push(url.to_string());
                Ok(())
            })
        };

        type_query(&mut app, "webhook");
        assert_eq!(press(&mut app, KeyCode::Enter), Action::Continue);

        assert_eq!(
            opened.lock().unwrap().as_slice(),
            ["https://github.com/introbotai/zyntra/pull/14"]
        );
        assert_eq!(app.opened, ["https://github.com/introbotai/zyntra/pull/14"]);
        assert_eq!(
            app.notice,
            Some(Notice::Opened("introbotai/zyntra#14".to_string()))
        );
    }

    #[test]
    fn a_failed_open_reports_an_error_without_quitting() {
        let pull_requests = pull_requests();
        let mut app = app(&pull_requests);
        app.open = Arc::new(|_| anyhow::bail!("no browser here"));

        assert_eq!(press(&mut app, KeyCode::Enter), Action::Continue);

        let Some(Notice::Error(message)) = &app.notice else {
            panic!("expected an error notice, got {:?}", app.notice);
        };
        assert!(message.contains("no browser here"));
        assert!(app.opened.is_empty());
    }

    #[test]
    fn the_footer_reports_the_opened_pull_request() {
        let pull_requests = pull_requests();
        let mut app = app(&pull_requests);

        press(&mut app, KeyCode::Enter);
        let content = render(&mut app, 120, 12);

        assert!(content.contains("opened introbotai/cluster-admin#416"));
    }

    #[test]
    fn the_footer_notice_stays_visible_on_narrow_terminals() {
        let pull_requests = pull_requests();
        let mut app = app(&pull_requests);

        press(&mut app, KeyCode::Enter);
        let content = render(&mut app, 80, 12);

        assert!(content.contains("opened introbotai/cluster-admin#416"));
    }

    #[test]
    fn escape_cancels_the_browser() {
        let pull_requests = pull_requests();
        let mut app = app(&pull_requests);

        assert_eq!(press(&mut app, KeyCode::Esc), Action::Cancel);
    }

    #[test]
    fn ctrl_c_cancels_the_browser() {
        let pull_requests = pull_requests();
        let mut app = app(&pull_requests);

        assert_eq!(
            press_with(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL),
            Action::Cancel
        );
    }

    #[test]
    fn arrows_wrap_around_the_list_ends() {
        let pull_requests = pull_requests();
        let mut app = app(&pull_requests);

        press(&mut app, KeyCode::Up);
        assert_eq!(
            app.state.selected(),
            Some(2),
            "up from the first row wraps to the last"
        );

        press(&mut app, KeyCode::Down);
        assert_eq!(
            app.state.selected(),
            Some(0),
            "down from the last row wraps to the first"
        );

        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.state.selected(), Some(2));

        press(&mut app, KeyCode::Down);
        assert_eq!(app.state.selected(), Some(0));
    }

    #[test]
    fn wrapping_scrolls_the_viewport_to_the_other_end() {
        let many = (1..=40)
            .map(|number| {
                pull_request(
                    number,
                    "cluster-admin",
                    &format!("item {number:02}"),
                    "vsnthdev",
                )
            })
            .collect::<Vec<_>>();
        let mut app = app(&many);
        let before = render(&mut app, 120, 12);
        assert!(before.contains("item 01"));

        press(&mut app, KeyCode::Up);
        let after = render(&mut app, 120, 12);

        assert_eq!(app.state.selected(), Some(39));
        assert!(after.contains("item 40"), "the last row scrolled into view");
        assert!(!after.contains("item 01"), "the first rows scrolled away");
    }

    #[test]
    fn paging_stops_at_the_list_ends() {
        let many = (1..=40)
            .map(|number| pull_request(number, "cluster-admin", "fix something", "vsnthdev"))
            .collect::<Vec<_>>();
        let mut app = app(&many);
        render(&mut app, 120, 12);

        for _ in 0..10 {
            press(&mut app, KeyCode::PageDown);
        }
        assert_eq!(app.state.selected(), Some(39));

        for _ in 0..10 {
            press(&mut app, KeyCode::PageUp);
        }
        assert_eq!(app.state.selected(), Some(0));
    }

    #[test]
    fn pad_left_aligns_within_display_columns() {
        assert_eq!(pad("ab", 5), "ab   ");
        assert_eq!(pad("abcdef", 3), "abcdef");
        assert_eq!(pad("你好", 5), "你好 ");
    }

    #[test]
    fn column_spacing_matches_its_literal() {
        assert_eq!(SPACES.len(), SPACING);
    }

    #[test]
    fn the_header_joins_columns_with_spaces() {
        let pull_requests = pull_requests();
        let app = app(&pull_requests);

        assert_eq!(
            app.table_header(),
            "  #  REPOSITORY      AUTHOR            TITLE"
        );
    }

    #[test]
    fn rows_join_columns_with_spaces_and_keep_the_title_whole() {
        let pull_requests = pull_requests();
        let app = app(&pull_requests);

        assert_eq!(
            app.row_line(2),
            " 14  zyntra          tanisheesh        forward webhook data"
        );
    }

    #[test]
    fn slice_takes_the_window_at_the_offset() {
        assert_eq!(slice("hello", 0, 10), "hello");
        assert_eq!(slice("hello world", 0, 8), "hello wo");
        assert_eq!(slice("hello world", 4, 8), "o world");
        assert_eq!(slice("hello", 5, 4), "");
        assert_eq!(slice("hello", 0, 0), "");
    }

    #[test]
    fn slice_keeps_wide_graphemes_intact() {
        assert_eq!(slice("你好世界", 1, 4), "你好");
        assert_eq!(slice("你好世界", 2, 4), "好世");
        assert_eq!(slice("🎊 deployments", 2, 6), " deplo");
        assert_eq!(slice("ab🎊", 0, 3), "ab");
    }

    #[test]
    fn renders_columns_in_the_requested_order() {
        let pull_requests = pull_requests();
        let mut app = app(&pull_requests);
        let content = render(&mut app, 120, 12);

        let mut position = 0;
        for header in ["#", "REPOSITORY", "AUTHOR", "TITLE"] {
            let found = content[position..]
                .find(header)
                .unwrap_or_else(|| panic!("missing {header} header after {position}: {content}"));
            position += found + header.len();
        }
    }

    #[test]
    fn renders_the_selection_marker_and_data() {
        let pull_requests = pull_requests();
        let mut app = app(&pull_requests);
        let content = render(&mut app, 120, 12);

        assert!(content.contains('▶'));
        assert!(content.contains("416"));
        assert!(content.contains("cluster-admin"));
        assert!(content.contains("show india company ranking completion"));
        assert!(!content.contains("UPDATED"));
    }

    #[test]
    fn cuts_long_titles_at_the_viewport_edge() {
        let mut pull_requests = pull_requests();
        pull_requests[0].title = "fix ".repeat(60);

        let mut app = app(&pull_requests);
        let content = render(&mut app, 120, 12);

        assert!(content.contains("fix fix"));
        assert!(!content.contains('…'), "nothing is ellipsized: {content}");
    }

    #[test]
    fn renders_a_hint_when_the_terminal_is_too_narrow() {
        let pull_requests = pull_requests();
        let mut app = app(&pull_requests);
        let content = render(&mut app, 13, 10);

        assert!(content.contains("too narrow"));
    }

    #[test]
    fn a_narrow_terminal_can_pan_to_reveal_later_columns() {
        let pull_requests = overflow_pull_requests();
        let mut app = app(&pull_requests);
        let content = render(&mut app, 30, 8);
        assert!(content.contains("REPOSITORY"));
        assert!(!content.contains("TITLE"));

        for _ in 0..3 {
            press_with(&mut app, KeyCode::Right, KeyModifiers::SHIFT);
        }
        let content = render(&mut app, 30, 8);
        assert!(content.contains("TITLE"));
    }

    #[test]
    fn renders_a_no_matches_state() {
        let pull_requests = pull_requests();
        let mut app = app(&pull_requests);
        type_query(&mut app, "zzz");
        let content = render(&mut app, 120, 12);

        assert!(content.contains("no pull requests match your search"));
    }

    #[test]
    fn renders_a_scrollbar_for_long_lists() {
        let many = (1..=40)
            .map(|number| pull_request(number, "cluster-admin", "fix something", "vsnthdev"))
            .collect::<Vec<_>>();
        let mut app = app(&many);
        let content = render(&mut app, 120, 12);

        assert!(content.contains('█'));
    }

    fn overflow_pull_requests() -> Vec<PullRequest> {
        vec![pull_request(
            1,
            "cluster-admin",
            &format!("{} zulutail", "fix ".repeat(40)),
            "tanisheesh",
        )]
    }

    #[test]
    fn shift_arrows_pan_the_whole_table() {
        let pull_requests = overflow_pull_requests();
        let mut app = app(&pull_requests);
        render(&mut app, 80, 8);
        assert!(app.max_offset > 0);

        press(&mut app, KeyCode::Right);
        assert_eq!(app.offset, 0, "plain right edits the query, not the pan");

        press_with(&mut app, KeyCode::Right, KeyModifiers::SHIFT);
        assert_eq!(app.offset, PAN_STEP);

        press_with(&mut app, KeyCode::Left, KeyModifiers::SHIFT);
        assert_eq!(app.offset, 0);
    }

    #[test]
    fn alt_arrows_pan_as_an_alias() {
        let pull_requests = overflow_pull_requests();
        let mut app = app(&pull_requests);
        render(&mut app, 80, 8);

        press_with(&mut app, KeyCode::Right, KeyModifiers::ALT);
        assert_eq!(app.offset, PAN_STEP);

        press_with(&mut app, KeyCode::Char('b'), KeyModifiers::ALT);
        assert_eq!(app.offset, 0);
    }

    #[test]
    fn shift_end_and_home_jump_to_the_edges_and_clamp() {
        let pull_requests = overflow_pull_requests();
        let mut app = app(&pull_requests);

        press_with(&mut app, KeyCode::End, KeyModifiers::SHIFT);
        assert_eq!(app.offset, 0, "nothing to pan before the first draw");

        render(&mut app, 80, 8);
        assert!(app.max_offset > 0);

        press_with(&mut app, KeyCode::End, KeyModifiers::SHIFT);
        assert_eq!(app.offset, app.max_offset);

        for _ in 0..10 {
            press_with(&mut app, KeyCode::Right, KeyModifiers::SHIFT);
        }
        assert_eq!(app.offset, app.max_offset);

        press_with(&mut app, KeyCode::Home, KeyModifiers::SHIFT);
        assert_eq!(app.offset, 0);

        for _ in 0..10 {
            press_with(&mut app, KeyCode::Left, KeyModifiers::SHIFT);
        }
        assert_eq!(app.offset, 0);
    }

    #[test]
    fn typing_resets_the_pan() {
        let pull_requests = overflow_pull_requests();
        let mut app = app(&pull_requests);
        render(&mut app, 80, 8);

        press_with(&mut app, KeyCode::Right, KeyModifiers::SHIFT);
        assert_eq!(app.offset, PAN_STEP);

        type_query(&mut app, "fix");
        assert_eq!(app.offset, 0);
    }

    #[test]
    fn panning_moves_the_whole_table() {
        let pull_requests = overflow_pull_requests();
        let mut app = app(&pull_requests);
        let before = render(&mut app, 80, 8);
        assert!(before.contains("cluster-admin"));
        assert!(!before.contains("zulutail"));

        press_with(&mut app, KeyCode::End, KeyModifiers::SHIFT);
        let after = render(&mut app, 80, 8);

        assert!(
            !after.contains("cluster-admin"),
            "the repository column scrolled off: {after}"
        );
        assert!(after.contains("zulutail"), "the title tail scrolled in");
        assert!(after.contains('◂'));
    }

    /// whether the first `symbol` on screen carries the dim modifier.
    fn is_dimmed(app: &mut App, width: u16, height: u16, symbol: &str) -> bool {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal.draw(|frame| app.draw(frame)).expect("draw");

        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .find(|cell| cell.symbol() == symbol)
            .unwrap_or_else(|| panic!("{symbol} is not on screen"))
            .modifier
            .contains(Modifier::DIM)
    }

    #[test]
    fn block_title_arrows_track_the_pan_direction() {
        let pull_requests = overflow_pull_requests();
        let mut app = app(&pull_requests);

        assert!(
            is_dimmed(&mut app, 80, 8, "◂"),
            "nothing is hidden on the left at offset 0"
        );
        assert!(!is_dimmed(&mut app, 80, 8, "▸"));

        press_with(&mut app, KeyCode::End, KeyModifiers::SHIFT);

        assert!(!is_dimmed(&mut app, 80, 8, "◂"));
        assert!(
            is_dimmed(&mut app, 80, 8, "▸"),
            "nothing is hidden on the right at the end"
        );
    }

    #[test]
    fn help_line_mentions_panning_only_when_the_table_overflows() {
        let short = pull_requests();
        let mut browser = app(&short);
        let content = render(&mut browser, 120, 12);
        assert!(content.contains("R refresh"));
        assert!(!content.contains("shift+←/→"));

        let long = overflow_pull_requests();
        let mut browser = app(&long);
        let content = render(&mut browser, 80, 8);
        assert!(content.contains("R refresh"));
        assert!(content.contains("shift+←/→"));
    }

    #[test]
    fn pressing_r_refreshes_on_a_thread_and_swaps_the_list() {
        let pull_requests = pull_requests();
        let mut app = app(&pull_requests);

        let calls = Arc::new(AtomicUsize::new(0));
        let refreshed = vec![pull_request(500, "fresh-repo", "brand new", "vsnthdev")];
        app.fetch = {
            let calls = Arc::clone(&calls);
            let refreshed = refreshed.clone();
            Arc::new(move || {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(refreshed.clone())
            })
        };

        assert_eq!(press(&mut app, KeyCode::Char('R')), Action::Continue);
        assert!(app.refresh.is_some(), "the refresh should be in flight");
        assert!(app.input.value().is_empty(), "R must not reach the query");

        // the fetch runs on a thread; wait briefly for it to land.
        for _ in 0..400 {
            app.check_refresh();
            if app.refresh.is_none() {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }

        assert!(app.refresh.is_none(), "the refresh never landed");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(app.pull_requests.len(), 1);
        assert_eq!(app.pull_requests[0].number, 500);
    }

    #[test]
    fn lowercase_r_still_types_into_the_query() {
        let pull_requests = pull_requests();
        let mut app = app(&pull_requests);

        press(&mut app, KeyCode::Char('r'));

        assert_eq!(app.input.value(), "r");
        assert!(app.refresh.is_none());
    }

    #[test]
    fn a_landed_refresh_keeps_the_highlighted_pull_request() {
        let pull_requests = pull_requests();
        let mut app = app(&pull_requests);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.pull_requests[app.selected_index().unwrap()].number, 14);

        let mut refreshed = vec![pull_request(500, "brand-new", "work", "vsnthdev")];
        refreshed.extend(pull_requests);
        let (sender, receiver) = mpsc::channel();
        app.refresh = Some(receiver);
        sender.send(Ok(refreshed)).unwrap();

        app.check_refresh();

        assert!(app.refresh.is_none());
        assert_eq!(app.pull_requests.len(), 4);
        let selected = app.pull_requests[app.selected_index().unwrap()].number;
        assert_eq!(selected, 14, "the same pull request should stay selected");
    }

    #[test]
    fn a_landed_refresh_resizes_columns_to_the_new_widest_repository() {
        let pull_requests = pull_requests();
        let mut app = app(&pull_requests);
        assert_eq!(app.repository_width, "api-exhibitors".len());

        let refreshed = vec![pull_request(
            1,
            "a-very-long-repository-name",
            "work",
            "vsnthdev",
        )];
        let (sender, receiver) = mpsc::channel();
        app.refresh = Some(receiver);
        sender.send(Ok(refreshed)).unwrap();

        app.check_refresh();

        assert_eq!(app.repository_width, "a-very-long-repository-name".len());
    }

    #[test]
    fn a_failed_refresh_keeps_the_data_and_shows_a_notice() {
        let pull_requests = pull_requests();
        let mut app = app(&pull_requests);

        let (sender, receiver) = mpsc::channel();
        app.refresh = Some(receiver);
        sender.send(Err(anyhow::anyhow!("network down"))).unwrap();

        app.check_refresh();

        assert!(app.refresh.is_none());
        assert_eq!(app.pull_requests.len(), 3, "the old list is kept");

        let Some(Notice::Error(message)) = &app.notice else {
            panic!("expected an error notice, got {:?}", app.notice);
        };
        assert!(message.contains("network down"));
    }

    #[test]
    fn refresh_is_ignored_while_one_is_in_flight() {
        let pull_requests = pull_requests();
        let mut app = app(&pull_requests);

        let (_sender, receiver) = mpsc::channel();
        app.refresh = Some(receiver);
        app.spinner = 4;

        press(&mut app, KeyCode::Char('R'));

        assert_eq!(app.spinner, 4, "the in-flight refresh was left alone");
    }

    #[test]
    fn starting_a_refresh_clears_the_previous_notice() {
        let pull_requests = pull_requests();
        let mut app = app(&pull_requests);
        app.notice = Some(Notice::Error("refresh failed".to_string()));

        press(&mut app, KeyCode::Char('R'));

        assert!(app.refresh.is_some());
        assert!(app.notice.is_none());
    }

    #[test]
    fn the_table_title_shows_a_spinner_while_refreshing() {
        let pull_requests = pull_requests();
        let mut app = app(&pull_requests);

        let (_sender, receiver) = mpsc::channel();
        app.refresh = Some(receiver);

        let content = render(&mut app, 120, 12);
        assert!(
            SPINNER.iter().any(|frame| content.contains(*frame)),
            "expected a spinner frame in: {content}"
        );
    }

    #[test]
    fn renders_an_empty_state_after_everything_closes() {
        let empty: Vec<PullRequest> = Vec::new();
        let mut app = app(&empty);
        let content = render(&mut app, 120, 12);

        assert!(content.contains("no open pull requests"));
    }
}
