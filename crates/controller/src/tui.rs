//! Minimal fullscreen host API frontend. Approval commands include displayed IDs.
use crate::{STOP, plain::escaped_json};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use goblins_controller::{
    Result,
    controller::Snapshot,
    host::{Client, decision},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout},
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
            event::EnableBracketedPaste
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
struct View {
    sessions: TableState,
    packages: TableState,
    details: bool,
    request_scroll: u16,
    line: String,
    notice: String,
}
impl View {
    fn draw(&mut self, f: &mut ratatui::Frame, snapshot: &Snapshot) {
        let areas = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(4),
            Constraint::Length(12),
            Constraint::Length(4),
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
                .block(border(" Packages "))
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
            .block(border(" Sandboxes "))
            .row_highlight_style(Style::default().reversed());
            f.render_stateful_widget(table, areas[1], &mut self.sessions);
        }
        let pending = snapshot
            .permissions
            .iter()
            .find(|p| p.state == "pending" && selected.is_some_and(|s| s.id == p.session))
            .or_else(|| snapshot.permissions.iter().find(|p| p.state == "pending"));
        let block = border(" Request · select its sandbox with arrows · PgUp/PgDn scroll ");
        let inner = block.inner(areas[2]);
        f.render_widget(block, areas[2]);
        if let Some(p) = pending {
            let parts = Layout::vertical([Constraint::Min(1), Constraint::Length(2)]).split(inner);
            let preview = p
                .preview
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| "Checking...".into());
            let text = format!(
                "Session: {}\nPackage: {}\nPreview: {}\nReason: {}",
                safe(&p.session),
                safe(&p.package),
                safe(&preview),
                safe(&p.reason)
            );
            f.render_widget(
                Paragraph::new(text)
                    .wrap(Wrap { trim: false })
                    .scroll((self.request_scroll, 0)),
                parts[0],
            );
            f.render_widget(
                Paragraph::new(format!("approve {}\ndeny {}", p.approval, p.approval)),
                parts[1],
            );
        } else {
            let latest = snapshot
                .permissions
                .iter()
                .rev()
                .find(|p| selected.is_some_and(|s| s.id == p.session));
            let outcome = latest
                .map(|p| {
                    format!(
                        "{}: {}\n{}",
                        safe(&p.package),
                        safe(&p.state),
                        safe(p.message.as_deref().unwrap_or(""))
                    )
                })
                .unwrap_or_else(|| self.notice.clone());
            let detail = selected
                .and_then(|s| s.detail.as_deref())
                .map(safe)
                .unwrap_or_default();
            f.render_widget(
                Paragraph::new(format!("{outcome}\n{detail}")).wrap(Wrap { trim: false }),
                inner,
            );
        }
        f.render_widget(
            Paragraph::new(format!(
                "> {}\nEnter command · empty Enter: packages · Esc: back/clear · Ctrl-C: close",
                self.line
            ))
            .block(border(" Command · exact approval token required ")),
            areas[3],
        );
    }
    fn move_selection(&mut self, delta: isize, snapshot: &Snapshot) {
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
}
pub fn serve(state: PathBuf) -> Result<()> {
    let mut client = Client::connect(&state)?;
    let mut subscription = Client::connect(&state)?.subscribe()?;
    if client.instance != subscription.snapshot.instance {
        return Err("daemon changed while connecting; reconnect".into());
    }
    let mut screen = Screen::open()?;
    let mut view = View {
        sessions: TableState::default().with_selected(0),
        packages: TableState::default().with_selected(0),
        details: false,
        request_scroll: 0,
        line: String::new(),
        notice: String::new(),
    };
    while !STOP.load(Ordering::Relaxed) {
        subscription.tick()?;
        screen.0.draw(|f| view.draw(f, &subscription.snapshot))?;
        if !event::poll(Duration::from_millis(20))? {
            continue;
        }
        if let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            match key.code {
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                KeyCode::Enter => {
                    if view.line == "quit" {
                        break;
                    }
                    if let Some((p, yes)) = decision(&view.line, &subscription.snapshot) {
                        view.notice = match client.decide(p, yes) {
                            Ok(v) => safe(&v.to_string()),
                            Err(e) => safe(&e.to_string()),
                        };
                    } else if view.line.is_empty() {
                        view.details = !view.details;
                    } else {
                        view.notice =
                            "No matching pending approval; use approve TOKEN or deny TOKEN".into();
                    }
                    view.line.clear();
                }
                KeyCode::Esc => {
                    view.line.clear();
                    view.details = false;
                }
                KeyCode::Backspace => {
                    view.line.pop();
                }
                KeyCode::Up => view.move_selection(-1, &subscription.snapshot),
                KeyCode::Down => view.move_selection(1, &subscription.snapshot),
                KeyCode::PageUp => view.request_scroll = view.request_scroll.saturating_sub(4),
                KeyCode::PageDown => {
                    view.request_scroll = view.request_scroll.saturating_add(4).min(4096)
                }
                KeyCode::Char(ch)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                        && ch.is_ascii()
                        && !ch.is_ascii_control()
                        && view.line.len() < 4096 =>
                {
                    view.line.push(ch)
                }
                _ => (),
            }
        }
        // Paste and mouse events never submit approval commands.
    }
    Ok(())
}
