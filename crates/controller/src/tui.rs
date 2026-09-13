//! Fullscreen frontend only. Layout, terminal input and colors stay out of the
//! controller library; every affirmative action carries its exact approval ID.
use super::{STOP, plain::escaped_json};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event as Input, KeyCode, KeyEventKind,
        KeyModifiers, MouseButton, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use goblins_controller::{
    Result,
    catalog::Preview,
    controller::{ApprovalId, Controller, Event},
};
use goblins_protocol::Request;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Cell, Paragraph, Row, Scrollbar, ScrollbarOrientation,
        ScrollbarState, Table, TableState,
    },
};
use std::{
    fs::{File, OpenOptions},
    path::PathBuf,
    sync::atomic::Ordering,
    time::Duration,
};

const TICK: Duration = Duration::from_millis(30);
#[derive(Clone, Copy)]
struct Colors {
    fg: Color,
    bg: Color,
    accent: Color,
    green: Color,
    yellow: Color,
    red: Color,
}
impl Colors {
    // Named colors refer to the terminal palette. Reset preserves its default
    // foreground/background; no theme probing or RGB fallback is needed.
    fn terminal() -> Self {
        Self {
            fg: Color::Reset,
            bg: Color::Reset,
            accent: Color::Blue,
            green: Color::Green,
            yellow: Color::Yellow,
            red: Color::Red,
        }
    }
    fn base(self) -> Style {
        Style::default().fg(self.fg).bg(self.bg)
    }
    fn border(self, title: String) -> Block<'static> {
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Plain)
            .border_style(Style::default().fg(self.accent))
            .title(title)
            .style(self.base())
    }
}
/// Best-effort restoration also runs on controller errors and caught unwinding.
struct Screen {
    terminal: Terminal<CrosstermBackend<File>>,
}
impl Screen {
    fn open() -> Result<Self> {
        let output = OpenOptions::new().read(true).write(true).open("/dev/tty")?;
        let mut screen = Self {
            terminal: Terminal::new(CrosstermBackend::new(output))?,
        };
        enable_raw_mode()?;
        execute!(
            screen.terminal.backend_mut(),
            EnterAlternateScreen,
            EnableMouseCapture,
            crossterm::event::EnableBracketedPaste
        )?;
        screen.terminal.hide_cursor()?;
        Ok(screen)
    }
}
impl Drop for Screen {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            crossterm::event::DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen,
            crossterm::cursor::Show
        );
    }
}
struct Pending {
    approval: ApprovalId,
    request: Request,
    sandbox: String,
    preview: Option<std::result::Result<Preview, String>>,
    decided: Option<bool>,
}
struct View {
    details: bool,
    table: TableState,
    pending: Option<Pending>,
    notice: String,
    yes_selected: bool,
    reason_scroll: usize,
    table_area: Rect,
    request_area: Rect,
    yes_area: Rect,
    no_area: Rect,
    row_count: usize,
}
fn safe(text: &str) -> String {
    let quoted = escaped_json(&text).unwrap_or_else(|_| "\"unavailable\"".into());
    quoted[1..quoted.len() - 1].into()
}
impl View {
    fn new() -> Self {
        Self {
            details: false,
            table: TableState::default().with_selected(0),
            pending: None,
            notice: "Run goblins run NAME in another terminal to connect.".into(),
            yes_selected: false,
            reason_scroll: 0,
            table_area: Rect::default(),
            request_area: Rect::default(),
            yes_area: Rect::default(),
            no_area: Rect::default(),
            row_count: 0,
        }
    }
    fn events(&mut self, events: Vec<Event>, controller: &Controller) {
        for event in events {
            match event {
                Event::Connected(name) => {
                    self.notice = format!("{} connected", safe(&name));
                    self.details = false;
                    self.table.select(Some(0));
                }
                Event::Stopped => {
                    self.details = false;
                    self.pending = None;
                    self.notice = "Sandbox stopped. Ready for another connection.".into();
                }
                Event::Request { request, approval } => {
                    self.pending = Some(Pending {
                        approval,
                        request,
                        sandbox: controller
                            .sandbox()
                            .map(|s| s.name.to_string())
                            .unwrap_or_default(),
                        preview: None,
                        decided: None,
                    });
                    self.yes_selected = false;
                    self.reason_scroll = 0;
                }
                Event::Preview { approval, result } => {
                    if let Some(p) = self.pending.as_mut().filter(|p| p.approval == approval) {
                        p.preview = Some(result);
                    }
                }
                Event::RequestGone(id) => {
                    if self.pending.as_ref().is_some_and(|p| p.approval == id) {
                        self.pending = None;
                        self.notice = "Request withdrawn: client disconnected.".into();
                    }
                }
                Event::DecisionFinished { approval, reply } => {
                    self.notice = format!(
                        "{}{}",
                        safe(&reply.status),
                        reply
                            .message
                            .as_ref()
                            .map(|m| format!(": {}", safe(m)))
                            .unwrap_or_default()
                    );
                    if self
                        .pending
                        .as_ref()
                        .is_some_and(|p| p.approval == approval)
                    {
                        self.pending = None;
                    }
                }
                Event::Result(_) => (), // An unrelated public client's error is not a decision.
                Event::Detail(detail) => {
                    self.notice = safe(&detail.chars().take(12000).collect::<String>());
                }
            }
        }
    }
    fn decide(&mut self, controller: &mut Controller, yes: bool) {
        if let Some(p) = self.pending.as_mut().filter(|p| p.decided.is_none()) {
            if controller.decide(p.approval, yes) {
                p.decided = Some(yes);
            } else {
                self.pending = None;
            }
        }
    }
    fn move_table(&mut self, delta: isize) {
        let next = self
            .table
            .selected()
            .unwrap_or(0)
            .saturating_add_signed(delta)
            .min(self.row_count.saturating_sub(1));
        self.table.select(Some(next));
    }
    fn input(&mut self, input: Input, controller: &mut Controller) -> bool {
        match input {
            Input::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Char('q') | KeyCode::Char('c')
                    if key.code == KeyCode::Char('q')
                        || key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    return true;
                }
                KeyCode::Char('y' | 'Y')
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    self.decide(controller, true)
                }
                KeyCode::Char('n' | 'N')
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    self.decide(controller, false)
                }
                KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                    self.yes_selected = !self.yes_selected
                }
                KeyCode::Enter if self.pending.is_some() => {
                    self.decide(controller, self.yes_selected)
                }
                KeyCode::Enter if !self.details && controller.sandbox().is_some() => {
                    self.details = true;
                    self.table = TableState::default().with_selected(0);
                }
                KeyCode::Esc | KeyCode::Backspace if self.details => {
                    self.details = false;
                    self.table = TableState::default().with_selected(0);
                }
                KeyCode::Up | KeyCode::Char('k') => self.move_table(-1),
                KeyCode::Down | KeyCode::Char('j') => self.move_table(1),
                KeyCode::Home => self.table.select(Some(0)),
                KeyCode::End => self.table.select(Some(self.row_count.saturating_sub(1))),
                KeyCode::PageDown if self.pending.is_some() => {
                    self.reason_scroll = self.reason_scroll.saturating_add(3)
                }
                KeyCode::PageUp if self.pending.is_some() => {
                    self.reason_scroll = self.reason_scroll.saturating_sub(3)
                }
                KeyCode::PageDown => self.move_table(10),
                KeyCode::PageUp => self.move_table(-10),
                _ => (),
            },
            Input::Mouse(mouse) => {
                let point = (mouse.column, mouse.row).into();
                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left)
                        if self.pending.as_ref().is_some_and(|p| p.decided.is_none()) =>
                    {
                        if self.yes_area.contains(point) {
                            self.decide(controller, true);
                        } else if self.no_area.contains(point) {
                            self.decide(controller, false);
                        }
                    }
                    MouseEventKind::ScrollDown if self.table_area.contains(point) => {
                        self.move_table(3)
                    }
                    MouseEventKind::ScrollUp if self.table_area.contains(point) => {
                        self.move_table(-3)
                    }
                    MouseEventKind::ScrollDown if self.request_area.contains(point) => {
                        self.reason_scroll += 3
                    }
                    MouseEventKind::ScrollUp if self.request_area.contains(point) => {
                        self.reason_scroll = self.reason_scroll.saturating_sub(3)
                    }
                    _ => (),
                }
            }
            // Pasted text must never become a quick y/n approval.
            Input::Paste(_)
            | Input::Resize(_, _)
            | Input::FocusGained
            | Input::FocusLost
            | Input::Key(_) => (),
        }
        false
    }
    fn draw(&mut self, f: &mut Frame, controller: &Controller) {
        let c = Colors::terminal();
        let area = f.area();
        f.render_widget(Block::default().style(c.base()), area);
        if area.width < 42 || area.height < 14 {
            self.yes_area = Rect::default();
            self.no_area = Rect::default();
            f.render_widget(Paragraph::new("Goblins\nTerminal too small: resize to at least 42 x 14.\nPending decisions remain available with y / n.\nq quits.").style(c.base()), area);
            return;
        }
        let bottom_height = if self.pending.is_some() {
            (area.height / 2).clamp(9, 15)
        } else {
            3
        };
        let sections = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(bottom_height),
            Constraint::Length(1),
        ])
        .split(area);
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" GOBLINS ", c.base().fg(c.accent).bold()),
                Span::raw("  Sandbox access controller"),
            ]))
            .style(c.base()),
            sections[0],
        );
        let mut rows = Vec::new();
        let mut title = " Connected sandboxes (0) ".to_string();
        if let Some(sandbox) = controller.sandbox() {
            let goblins_controller::controller::Sandbox {
                name,
                identity,
                initial_packages: initial,
                granted_packages: granted,
                status,
            } = sandbox;
            let pid = identity
                .map(|i| i.pid.to_string())
                .unwrap_or_else(|| "-".into());
            if self.details {
                title = format!(
                    " {} · PID {} · {} · Packages ({}) ",
                    safe(name),
                    pid,
                    status,
                    initial.len() + granted.len()
                );
                for (package, access) in initial
                    .iter()
                    .map(|p| (p, "Startup"))
                    .chain(granted.iter().map(|p| (p, "Granted")))
                {
                    rows.push(Row::new(vec![
                        Cell::from(safe(package)),
                        Cell::from(access).style(Style::default().fg(c.green)),
                    ]));
                }
                if rows.is_empty() {
                    rows.push(Row::new(["No packages granted yet", "-"]));
                }
            } else {
                title = " Connected sandboxes (1) ".into();
                rows.push(Row::new(vec![
                    Cell::from(safe(name)),
                    Cell::from(pid),
                    Cell::from(status).style(Style::default().fg(if status == "Approval" {
                        c.yellow
                    } else {
                        c.green
                    })),
                    Cell::from(format!(
                        "{} startup / {} granted",
                        initial.len(),
                        granted.len()
                    )),
                ]));
            }
        }
        self.row_count = rows.len();
        self.table_area = sections[1];
        let block = c.border(title);
        if rows.is_empty() {
            f.render_widget(
                Paragraph::new(
                    "\n  No connected sandbox\n  Run goblins run NAME in another terminal.",
                )
                .block(block),
                sections[1],
            );
        } else {
            let (widths, headers) = if self.details {
                (
                    vec![Constraint::Min(20), Constraint::Length(12)],
                    vec!["Package", "Access"],
                )
            } else {
                (
                    vec![
                        Constraint::Min(10),
                        Constraint::Length(8),
                        Constraint::Length(10),
                        Constraint::Length(24),
                    ],
                    vec!["Sandbox", "PID", "State", "Packages"],
                )
            };
            let table = Table::new(rows, widths)
                .header(
                    Row::new(headers)
                        .style(c.base().fg(c.accent).bold())
                        .bottom_margin(1),
                )
                .block(block)
                .column_spacing(2)
                .row_highlight_style(c.base().reversed())
                .highlight_symbol("› ");
            f.render_stateful_widget(table, sections[1], &mut self.table);
            // Keep borders intact when every row fits (including the overview).
            let visible_rows = sections[1].height.saturating_sub(4) as usize;
            if self.row_count > visible_rows {
                let mut scroll = ScrollbarState::new(self.row_count)
                    .position(self.table.selected().unwrap_or(0));
                f.render_stateful_widget(
                    Scrollbar::new(ScrollbarOrientation::VerticalRight),
                    sections[1],
                    &mut scroll,
                );
            }
        }
        self.request_area = sections[3];
        self.yes_area = Rect::default();
        self.no_area = Rect::default();
        if let Some(p) = &self.pending {
            let block = c
                .border(if p.decided.is_some() {
                    " Applying decision ".into()
                } else {
                    " Package request ".into()
                })
                .border_style(Style::default().fg(c.yellow));
            let inner = block.inner(sections[3]);
            f.render_widget(block, sections[3]);
            let body = Layout::vertical([Constraint::Min(1), Constraint::Length(3)]).split(inner);
            let info = match &p.preview {
                None => "In host store: Checking...\nDownload: Checking...".into(),
                Some(Err(_)) => {
                    "In host store: Unknown\nDownload: Unknown (preview unavailable)".into()
                }
                Some(Ok(p)) => format!(
                    "In host store: {}\nDownload: {}{}",
                    if p.in_store { "Yes" } else { "No" },
                    p.download.as_deref().unwrap_or("Unknown"),
                    if p.build_required {
                        " + build required (total unknown)"
                    } else {
                        ""
                    }
                ),
            };
            let content = format!(
                "Sandbox: {}\nPackage: {}\n{}\nReason: {}{}",
                safe(&p.sandbox),
                safe(&p.request.package),
                info,
                safe(&p.request.reason),
                p.preview
                    .as_ref()
                    .and_then(|v| v.as_ref().err())
                    .map(|e| format!("\nPreview: {}", safe(e)))
                    .unwrap_or_default()
            );
            let lines = wrap(&content, body[0].width as usize);
            self.reason_scroll = self
                .reason_scroll
                .min(lines.len().saturating_sub(body[0].height as usize));
            f.render_widget(
                Paragraph::new(
                    lines
                        .into_iter()
                        .skip(self.reason_scroll)
                        .map(Line::from)
                        .collect::<Vec<_>>(),
                ),
                body[0],
            );
            if let Some(yes) = p.decided {
                f.render_widget(
                    Paragraph::new(if yes {
                        " Approved · Realizing and mounting package…"
                    } else {
                        " Denied · Sending response…"
                    })
                    .style(c.base().fg(if yes { c.green } else { c.red })),
                    body[1],
                );
            } else {
                let buttons = Layout::horizontal([
                    Constraint::Length(13),
                    Constraint::Length(2),
                    Constraint::Length(13),
                    Constraint::Min(0),
                ])
                .split(body[1]);
                self.yes_area = buttons[0];
                self.no_area = buttons[2];
                for (rect, label, color, selected) in [
                    (buttons[0], " Yes [y] ", c.green, self.yes_selected),
                    (buttons[2], " No [n] ", c.red, !self.yes_selected),
                ] {
                    let style = c.base().fg(color).bold();
                    f.render_widget(
                        Paragraph::new(label)
                            .centered()
                            .block(Block::bordered().border_type(BorderType::Plain))
                            .style(if selected { style.reversed() } else { style }),
                        rect,
                    );
                }
            }
        } else {
            f.render_widget(
                Paragraph::new(self.notice.clone()).block(c.border(" Activity ".into())),
                sections[3],
            );
        }
        let help = if self.pending.is_some() {
            if area.width >= 90 {
                " ↑↓ scroll  PgUp/Dn request  Tab buttons  Enter select  y/n decide  Esc back  q quit"
            } else {
                " y/n decide  Tab/Enter select  q quit"
            }
        } else if self.details {
            " ↑↓ scroll  PgUp/Dn page  Esc back  q quit"
        } else {
            " ↑↓ select  Enter details  q quit"
        };
        f.render_widget(Paragraph::new(help).style(c.base().dim()), sections[4]);
    }
}
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    text.lines()
        .flat_map(|line| {
            // safe() produces ASCII, so byte boundaries here are character boundaries.
            if line.is_empty() {
                vec![String::new()]
            } else {
                line.as_bytes()
                    .chunks(width)
                    .map(|c| String::from_utf8_lossy(c).into_owned())
                    .collect()
            }
        })
        .collect()
}
pub fn serve(state: PathBuf, workspace: Option<PathBuf>) -> Result<()> {
    let mut controller = Controller::new(&state, workspace)?;
    let mut screen = Screen::open()?;
    let mut view = View::new();
    while !STOP.load(Ordering::Relaxed) {
        view.events(controller.tick()?, &controller);
        screen.terminal.draw(|f| view.draw(f, &controller))?;
        if event::poll(TICK)? && view.input(event::read()?, &mut controller) {
            break;
        }
    }
    Ok(())
}
