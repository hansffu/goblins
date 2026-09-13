//! Host API approval frontend. Interactions retain the last displayed request.
use crate::{STOP, plain::escaped_json};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use goblins_controller::{
    Result,
    controller::{PermissionRecord, Snapshot},
    host::Client,
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    widgets::{Block, Cell, Paragraph, Row, Table, TableState, Wrap},
};
use std::{
    fs::{File, OpenOptions},
    path::PathBuf,
    sync::atomic::Ordering,
    time::Duration,
};
struct Screen(Terminal<CrosstermBackend<File>>);
impl Screen {
    fn open() -> Result<Self> {
        let out = OpenOptions::new().read(true).write(true).open("/dev/tty")?;
        let mut s = Self(Terminal::new(CrosstermBackend::new(out))?);
        enable_raw_mode()?;
        execute!(
            s.0.backend_mut(),
            EnterAlternateScreen,
            event::EnableBracketedPaste,
            event::EnableMouseCapture
        )?;
        Ok(s)
    }
}
impl Drop for Screen {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            self.0.backend_mut(),
            event::DisableBracketedPaste,
            event::DisableMouseCapture,
            LeaveAlternateScreen,
            crossterm::cursor::Show
        );
    }
}
fn safe(s: &str) -> String {
    let s = escaped_json(&s).unwrap_or_default();
    s.get(1..s.len().saturating_sub(1))
        .unwrap_or_default()
        .into()
}
#[derive(Clone, Copy, PartialEq)]
enum Focus {
    Sandboxes,
    Requests,
}
struct View {
    sessions: TableState,
    packages: TableState,
    requests: TableState,
    selected_request: Option<String>,
    focus: Focus,
    details: bool,
    popup_open: bool,
    request_scroll: u16,
    yes: bool,
    notice: String,
    presented: Option<PermissionRecord>,
    buttons: [Rect; 2],
    pressed: Option<(String, bool)>,
}
impl View {
    fn new() -> Self {
        Self {
            sessions: TableState::default().with_selected(0),
            packages: TableState::default().with_selected(0),
            requests: TableState::default(),
            selected_request: None,
            focus: Focus::Sandboxes,
            details: false,
            popup_open: true,
            request_scroll: 0,
            yes: false,
            notice: String::new(),
            presented: None,
            buttons: [Rect::default(); 2],
            pressed: None,
        }
    }
    fn sync(&mut self, snapshot: &Snapshot) {
        if self.selected_request.is_none() {
            self.selected_request = snapshot
                .permissions
                .iter()
                .find(|p| p.state == "pending")
                .or_else(|| snapshot.permissions.last())
                .map(|p| p.id.clone());
        }
        if self.presented.as_ref().is_some_and(|old| {
            old.state == "pending" && Some(&old.id) == self.selected_request.as_ref()
        }) && snapshot
            .permissions
            .iter()
            .any(|p| Some(&p.id) == self.selected_request.as_ref() && p.approved.is_some())
        {
            self.popup_open = false;
        }
        // Keep the selected identity after withdrawal/decision/eviction. Never
        // replace a displayed request with its successor underneath queued input.
        self.requests.select(
            snapshot
                .permissions
                .iter()
                .position(|p| Some(&p.id) == self.selected_request.as_ref()),
        );
    }
    fn draw(&mut self, f: &mut ratatui::Frame, snapshot: &Snapshot) {
        self.sync(snapshot);
        let has_request = self.selected_request.is_some();
        let popup = has_request && self.popup_open;
        let areas = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(if has_request {
                (snapshot.permissions.len() as u16).clamp(1, 4) + 2
            } else {
                0
            }),
            Constraint::Length(if popup { 11 } else { 0 }),
            Constraint::Length(2),
        ])
        .split(f.area());
        let border = |title: &str| {
            let style = if std::env::var_os("NO_COLOR").is_some() {
                Style::default()
            } else {
                Style::default().fg(Color::Blue)
            };
            Block::bordered()
                .title(title.to_string())
                .border_style(style)
        };
        f.render_widget(
            Paragraph::new(" Goblins · daemon approvals · sessions survive frontend closure"),
            areas[0],
        );
        let selected = snapshot.sessions.get(self.sessions.selected().unwrap_or(0));
        if self.details {
            let rows: Vec<Row> = selected
                .map(|s| {
                    s.initial_packages
                        .iter()
                        .map(|p| (p, "Startup"))
                        .chain(s.packages.iter().map(|p| (p, "Granted")))
                        .map(|(package, access)| {
                            Row::new(vec![Cell::from(safe(package)), Cell::from(access)])
                        })
                        .collect()
                })
                .unwrap_or_default();
            let table = Table::new(rows, [Constraint::Min(20), Constraint::Length(12)])
                .block(border(if self.focus == Focus::Sandboxes {
                    " Packages [focused] "
                } else {
                    " Packages "
                }))
                .row_highlight_style(Style::default().reversed());
            f.render_stateful_widget(table, areas[1], &mut self.packages);
        } else {
            let rows = snapshot.sessions.iter().map(|s| {
                Row::new(vec![
                    Cell::from(safe(&s.name)),
                    Cell::from(
                        s.identity
                            .as_ref()
                            .map(|i| i.pid.to_string())
                            .unwrap_or_default(),
                    ),
                    Cell::from(s.state.clone()),
                    Cell::from(format!(
                        "{} startup / {} granted",
                        s.initial_packages.len(),
                        s.packages.len()
                    )),
                ])
            });
            let table = Table::new(
                rows,
                [
                    Constraint::Min(12),
                    Constraint::Length(9),
                    Constraint::Length(12),
                    Constraint::Length(25),
                ],
            )
            .block(border(if self.focus == Focus::Sandboxes {
                " Sandboxes [focused] "
            } else {
                " Sandboxes "
            }))
            .row_highlight_style(Style::default().reversed());
            f.render_stateful_widget(table, areas[1], &mut self.sessions);
        }
        let rows = snapshot.permissions.iter().map(|p| {
            let name = snapshot
                .sessions
                .iter()
                .find(|s| s.id == p.session)
                .map(|s| s.name.as_str())
                .unwrap_or(&p.session);
            Row::new(vec![safe(name), safe(&p.package), safe(&p.state)])
        });
        let table = Table::new(
            rows,
            [
                Constraint::Min(15),
                Constraint::Min(20),
                Constraint::Length(12),
            ],
        )
        .block(border(if self.focus == Focus::Requests {
            " Requests [focused] "
        } else {
            " Requests "
        }))
        .row_highlight_style(Style::default().reversed());
        f.render_stateful_widget(table, areas[2], &mut self.requests);
        self.presented = snapshot
            .permissions
            .iter()
            .find(|p| self.popup_open && Some(&p.id) == self.selected_request.as_ref())
            .cloned();
        self.buttons = [Rect::default(); 2];
        if popup {
            let block = border(" Permission request · PgUp/PgDn scroll ");
            let inner = block.inner(areas[3]);
            f.render_widget(block, areas[3]);
            let parts = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
            if let Some(p) = &self.presented {
                let name = snapshot
                    .sessions
                    .iter()
                    .find(|s| s.id == p.session)
                    .map(|s| s.name.as_str())
                    .unwrap_or(&p.session);
                let preview = match &p.preview {
                    None => "Checking...".into(),
                    Some(v) if v.get("error").is_some() => format!(
                        "Unknown: {}",
                        v["error"].as_str().unwrap_or("preview failed")
                    ),
                    Some(v) => format!(
                        "Host store: {} · Download: {} · Build: {}",
                        if v["in_store"] == true {
                            "available"
                        } else {
                            "missing"
                        },
                        v["download"].as_str().unwrap_or("Unknown"),
                        if v["build_required"] == true {
                            "required (total cost unknown)"
                        } else {
                            "not required"
                        }
                    ),
                };
                let text = format!(
                    "Sandbox: {}\nPackage: {}\n{}\nReason: {}\nStatus: {}{}",
                    safe(name),
                    safe(&p.package),
                    safe(&preview),
                    safe(&p.reason),
                    safe(&p.state),
                    p.message
                        .as_ref()
                        .map(|m| format!(" · {}", safe(m)))
                        .unwrap_or_default()
                );
                f.render_widget(
                    Paragraph::new(text)
                        .wrap(Wrap { trim: false })
                        .scroll((self.request_scroll, 0)),
                    parts[0],
                );
                let buttons = Layout::horizontal([
                    Constraint::Length(10),
                    Constraint::Length(10),
                    Constraint::Min(0),
                ])
                .split(parts[1]);
                if p.state == "pending" {
                    self.buttons = [buttons[0], buttons[1]];
                    for (i, label) in ["[ No ]", "[ Yes ]"].iter().enumerate() {
                        let mut style = Style::default();
                        if std::env::var_os("NO_COLOR").is_none() {
                            style = style.fg(if i == 0 { Color::Red } else { Color::Green });
                        }
                        if self.focus == Focus::Requests && self.yes == (i == 1) {
                            style = style.reversed();
                        }
                        f.render_widget(Paragraph::new(*label).style(style), buttons[i]);
                    }
                } else {
                    f.render_widget(
                        Paragraph::new("Request finished · select another request with Up/Down"),
                        parts[1],
                    );
                }
            } else {
                f.render_widget(
                    Paragraph::new(
                        "Request no longer retained · select another request with Up/Down",
                    ),
                    parts[0],
                );
            }
        }
        f.render_widget(Paragraph::new(format!(
            "Tab: focus · ↑/↓: select · ←/→: No/Yes · Enter: activate · y/n: decide · q: quit\n{}", self.notice)), areas[4]);
    }
    fn move_selection(&mut self, delta: isize, snapshot: &Snapshot) {
        if self.focus == Focus::Requests {
            if !snapshot.permissions.is_empty() {
                let i = self
                    .requests
                    .selected()
                    .unwrap_or(0)
                    .saturating_add_signed(delta)
                    .min(snapshot.permissions.len() - 1);
                self.requests.select(Some(i));
                self.selected_request = Some(snapshot.permissions[i].id.clone());
                self.yes = false;
                self.popup_open = true;
                self.request_scroll = 0;
            }
            return;
        }
        let count = if self.details {
            snapshot
                .sessions
                .get(self.sessions.selected().unwrap_or(0))
                .map(|s| s.initial_packages.len() + s.packages.len())
                .unwrap_or(0)
        } else {
            snapshot.sessions.len()
        };
        let table = if self.details {
            &mut self.packages
        } else {
            &mut self.sessions
        };
        table.select(Some(
            table
                .selected()
                .unwrap_or(0)
                .saturating_add_signed(delta)
                .min(count.saturating_sub(1)),
        ));
        self.request_scroll = 0;
    }
    fn decide(&mut self, client: &mut Client, yes: bool) {
        if self.focus != Focus::Requests {
            return;
        }
        let Some(p) = self
            .presented
            .as_ref()
            .filter(|p| p.state == "pending" && Some(&p.id) == self.selected_request.as_ref())
        else {
            return;
        };
        self.notice = match client.decide(p, yes) {
            Ok(_) => {
                self.popup_open = false;
                if yes {
                    "Approved; waiting for package readiness"
                } else {
                    "Denied"
                }
                .into()
            }
            Err(e) => safe(&e.to_string()),
        };
        // No second event in the same input batch can submit this interaction.
        self.presented = None;
    }
    fn input(&mut self, input: Event, snapshot: &Snapshot, client: &mut Client) -> bool {
        match input {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return false;
                }
                KeyCode::Char('q') => return false,
                KeyCode::Tab | KeyCode::BackTab => {
                    self.focus = if self.focus == Focus::Sandboxes {
                        Focus::Requests
                    } else {
                        Focus::Sandboxes
                    };
                    self.yes = false;
                }
                KeyCode::Up => self.move_selection(-1, snapshot),
                KeyCode::Down => self.move_selection(1, snapshot),
                KeyCode::Left if self.focus == Focus::Requests => self.yes = false,
                KeyCode::Right if self.focus == Focus::Requests => self.yes = true,
                KeyCode::Enter if self.focus == Focus::Requests => {
                    if self.popup_open {
                        self.decide(client, self.yes);
                    } else {
                        self.popup_open = true;
                        self.yes = false;
                    }
                }
                KeyCode::Enter => self.details = !self.details,
                KeyCode::Char('y') => self.decide(client, true),
                KeyCode::Char('n') => self.decide(client, false),
                KeyCode::Esc => {
                    self.popup_open = false;
                    self.presented = None;
                    self.pressed = None;
                    self.details = false;
                }
                KeyCode::Backspace => self.details = false,
                KeyCode::PageUp => self.request_scroll = self.request_scroll.saturating_sub(4),
                KeyCode::PageDown => {
                    self.request_scroll = self.request_scroll.saturating_add(4).min(4096)
                }
                _ => (),
            },
            Event::Mouse(mouse) => {
                let hit = self
                    .buttons
                    .iter()
                    .position(|r| r.contains((mouse.column, mouse.row).into()));
                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        self.pressed = hit.and_then(|i| {
                            self.presented
                                .as_ref()
                                .filter(|p| p.state == "pending")
                                .map(|p| (p.id.clone(), i == 1))
                        });
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        if let Some((id, yes)) = self.pressed.take()
                            && hit == Some(usize::from(yes))
                            && self.presented.as_ref().is_some_and(|p| p.id == id)
                        {
                            self.focus = Focus::Requests;
                            self.decide(client, yes);
                        }
                    }
                    _ => (),
                }
            }
            // Bracketed paste never becomes key/button input.
            _ => (),
        }
        true
    }
}
pub fn serve(state: PathBuf) -> Result<()> {
    let mut client = Client::connect(&state)?;
    let mut subscription = Client::connect(&state)?.subscribe()?;
    if client.instance != subscription.snapshot.instance {
        return Err("daemon changed while connecting; reconnect".into());
    }
    let mut screen = Screen::open()?;
    let mut view = View::new();
    screen.0.draw(|f| view.draw(f, &subscription.snapshot))?;
    while !STOP.load(Ordering::Relaxed) {
        // Consume queued events against the last rendered identity BEFORE any
        // snapshot or selection is presented. Navigation + activation in one
        // batch cannot activate an as-yet-unseen replacement. Bound each batch,
        // but never redraw while more input remains queued.
        if event::poll(Duration::from_millis(20))? {
            for _ in 0..256 {
                if !view.input(event::read()?, &subscription.snapshot, &mut client) {
                    return Ok(());
                }
                if !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
        subscription.tick()?;
        if !event::poll(Duration::ZERO)? {
            screen.0.draw(|f| view.draw(f, &subscription.snapshot))?;
        }
    }
    Ok(())
}
