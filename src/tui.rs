//! The terminal view of a run: overall progress, each state, live agents, the model ladder,
//! and the log. Keys pause and resume, change concurrency, sign in, and quit.

use crate::scheduler::Scheduler;
use crate::view::{View, human};
use erisagentauth::claude::ClaudeAccount;
use erisagentauth::openai::{Method, OpenAiAccount};
use erisagentauth::{AccountSnapshot, Event, Status as AccountStatus};
use ratatui::crossterm::event::{self, Event as TermEvent, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Gauge, Paragraph, Row, Table, Wrap};
use ratatui::{DefaultTerminal, Frame};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A sign-in under way, shown over the view.
#[derive(Default)]
struct Login {
    url: String,
    code: Option<String>,
    message: Option<String>,
}

pub async fn run(
    scheduler: Arc<Scheduler>,
    account: OpenAiAccount,
    claude: ClaudeAccount,
    mut events: tokio::sync::broadcast::Receiver<Event>,
) -> anyhow::Result<()> {
    let login: Arc<Mutex<Option<Login>>> = Arc::default();
    {
        let login = login.clone();
        let scheduler = scheduler.clone();
        tokio::spawn(async move {
            while let Ok(event) = events.recv().await {
                if let Event::LoginCompleted { ok, error, .. } = event {
                    let mut login = login.lock().unwrap();
                    if ok {
                        *login = None;
                        scheduler.work.log("signed in with ChatGPT");
                        scheduler.paused.store(false, Ordering::SeqCst);
                        scheduler.poke();
                    } else if let Some(login) = login.as_mut() {
                        login.message = Some(format!("Sign-in failed: {}", error.unwrap_or_default()));
                    }
                }
            }
        });
    }
    let runner = {
        let scheduler = scheduler.clone();
        tokio::spawn(async move { scheduler.run().await })
    };
    let mut terminal = ratatui::init();
    let result = drive(&mut terminal, &scheduler, &account, &claude, &login, &runner).await;
    ratatui::restore();
    scheduler.stop().await;
    result
}

async fn drive(
    terminal: &mut DefaultTerminal,
    scheduler: &Arc<Scheduler>,
    account: &OpenAiAccount,
    claude: &ClaudeAccount,
    login: &Arc<Mutex<Option<Login>>>,
    runner: &tokio::task::JoinHandle<anyhow::Result<()>>,
) -> anyhow::Result<()> {
    let mut log_scroll = 0usize;
    loop {
        let view = View::of(scheduler);
        let logins = logins(&account.snapshot(), &claude.snapshot());
        terminal.draw(|frame| {
            draw(frame, &view, logins, login.lock().unwrap().as_ref(), runner.is_finished(), log_scroll)
        })?;
        let poll = tokio::task::spawn_blocking(|| -> std::io::Result<Option<TermEvent>> {
            if event::poll(Duration::from_millis(400))? { Ok(Some(event::read()?)) } else { Ok(None) }
        })
        .await??;
        let Some(TermEvent::Key(key)) = poll else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Char('q') => return Ok(()),
            KeyCode::Esc if login.lock().unwrap().take().is_some() => account.cancel().await,
            KeyCode::Char('p') => {
                let paused = !scheduler.paused.load(Ordering::SeqCst);
                scheduler.paused.store(paused, Ordering::SeqCst);
                scheduler.work.log(if paused { "paused: no new jobs start" } else { "resumed" });
                scheduler.poke();
            }
            KeyCode::Char('+') | KeyCode::Char('=') => {
                let mut settings = scheduler.settings.lock().unwrap();
                settings.concurrency += 1;
                scheduler.poke();
            }
            KeyCode::Char('-') => {
                let mut settings = scheduler.settings.lock().unwrap();
                settings.concurrency = settings.concurrency.saturating_sub(1).max(1);
            }
            KeyCode::Char(c @ ('l' | 'L')) => {
                let method = if c == 'l' { Method::DeviceCode } else { Method::Browser };
                match account.login(method).await {
                    Ok(start) => {
                        if method == Method::Browser {
                            let _ = std::process::Command::new("xdg-open")
                                .arg(&start.url)
                                .stdout(std::process::Stdio::null())
                                .stderr(std::process::Stdio::null())
                                .spawn();
                        }
                        *login.lock().unwrap() = Some(Login { url: start.url, code: start.user_code, message: None });
                    }
                    Err(error) => scheduler.work.log(format!("sign-in failed: {error:#}")),
                }
            }
            KeyCode::Up => log_scroll = log_scroll.saturating_add(1),
            KeyCode::Down => log_scroll = log_scroll.saturating_sub(1),
            _ => {}
        }
    }
}

/// The header's word on each login: ChatGPT's, with how to sign in when it is not, and
/// Claude's when it is bound or failing.
fn logins(chatgpt: &AccountSnapshot, claude: &AccountSnapshot) -> Vec<Span<'static>> {
    let mut spans = vec![if chatgpt.status == AccountStatus::Connected {
        Span::raw(" ChatGPT ✓ ")
    } else {
        Span::styled(" ChatGPT: press l to sign in ", Style::new().fg(Color::Red))
    }];
    match claude.status {
        AccountStatus::Connected => spans.push(Span::raw(" Claude ✓ ")),
        AccountStatus::Error => spans.push(Span::styled(
            format!(" Claude: {} ", claude.detail.as_deref().unwrap_or("error")),
            Style::new().fg(Color::Red),
        )),
        _ => {}
    }
    spans
}

fn draw(
    frame: &mut Frame,
    view: &View,
    logins: Vec<Span<'static>>,
    login: Option<&Login>,
    finished: bool,
    log_scroll: usize,
) {
    let [header, gauge, middle, log, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Min(10),
        Constraint::Length(10),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let state = if finished {
        Span::styled(" finished ", Style::new().bg(Color::Green).fg(Color::Black))
    } else if view.paused {
        Span::styled(" paused ", Style::new().bg(Color::Yellow).fg(Color::Black))
    } else {
        Span::styled(" running ", Style::new().bg(Color::Blue).fg(Color::White))
    };
    let mut spans = vec![Span::styled(" ErisCountyGISAdapters ", Style::new().add_modifier(Modifier::BOLD)), state];
    spans.extend(logins);
    frame.render_widget(
        Line::from(
            spans
                .into_iter()
                .chain([Span::raw(format!(
                    " concurrency {} · tokens {} in ({} cached) / {} out",
                    view.concurrency,
                    human(view.tokens.input),
                    human(view.tokens.cached_input),
                    human(view.tokens.output)
                ))])
                .collect::<Vec<_>>(),
        ),
        header,
    );

    let t = &view.totals;
    let done = t.covered + t.unavailable;
    let ratio = if t.total() == 0 { 0.0 } else { done as f64 / t.total() as f64 };
    frame.render_widget(
        Gauge::default()
            .block(Block::bordered().title(format!(
                " {} covered · {} unavailable · {} running · {} failed · {} pending · of {} ",
                t.covered,
                t.unavailable,
                t.running,
                t.failed,
                t.pending,
                t.total()
            )))
            .gauge_style(Style::new().fg(Color::Green))
            .ratio(ratio.clamp(0.0, 1.0))
            .label(format!("{:.1}%", ratio * 100.0)),
        gauge,
    );

    let [left, right] = Layout::horizontal([Constraint::Length(46), Constraint::Min(40)]).areas(middle);
    draw_states(frame, left, view);
    let [agents, rungs] =
        Layout::vertical([Constraint::Min(5), Constraint::Length(view.rungs.len() as u16 + 3)]).areas(right);
    draw_agents(frame, agents, view);
    draw_rungs(frame, rungs, view);

    let lines: Vec<Line> =
        view.log.iter().skip(log_scroll).take(log.height as usize).map(|l| Line::raw(l.as_str())).rev().collect();
    frame.render_widget(Paragraph::new(lines).block(Block::bordered().title(" log (↑/↓) ")), log);

    frame.render_widget(
        Line::from(
            " p pause/resume · +/- concurrency · l sign in (device code) · L sign in (browser) · esc close · q quit ",
        )
        .dim(),
        footer,
    );

    if let Some(login) = login {
        let area = centered(frame.area(), 72, 9);
        frame.render_widget(Clear, area);
        let mut lines =
            vec![Line::raw("Sign in with ChatGPT"), Line::raw(""), Line::raw(format!("Open: {}", login.url))];
        if let Some(code) = &login.code {
            lines.push(Line::from(vec![
                Span::raw("Enter the code: "),
                Span::styled(code.clone(), Style::new().add_modifier(Modifier::BOLD)),
            ]));
        }
        if let Some(message) = &login.message {
            lines.push(Line::styled(message.clone(), Style::new().fg(Color::Red)));
        }
        lines.push(Line::raw(""));
        lines.push(Line::raw("Waiting… (esc cancels)").dim());
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(Block::bordered()), area);
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect { x: area.x + (area.width - width) / 2, y: area.y + (area.height - height) / 2, width, height }
}

fn draw_states(frame: &mut Frame, area: Rect, view: &View) {
    let rows = view.states.iter().map(|(state, c, searching)| {
        let name = if *searching { format!("{state} ⟳") } else { state.clone() };
        let style = if c.covered + c.unavailable == c.total() { Style::new().fg(Color::Green) } else { Style::new() };
        Row::new(vec![
            name,
            c.total().to_string(),
            c.covered.to_string(),
            c.unavailable.to_string(),
            c.running.to_string(),
            c.failed.to_string(),
        ])
        .style(style)
    });
    let widths = [
        Constraint::Length(6),
        Constraint::Length(7),
        Constraint::Length(8),
        Constraint::Length(7),
        Constraint::Length(5),
        Constraint::Length(6),
    ];
    frame.render_widget(
        Table::new(rows, widths)
            .header(Row::new(vec!["state", "total", "covered", "unavail", "run", "failed"]).bold())
            .block(Block::bordered().title(" states ")),
        area,
    );
}

fn draw_agents(frame: &mut Frame, area: Rect, view: &View) {
    let rows = view.agents.iter().map(|a| {
        Row::new(vec![
            a.job.clone(),
            format!("{} {}", a.rung, a.model),
            format!("{}:{:02}", a.seconds / 60, a.seconds % 60),
            format!("{}/{}", human(a.tokens.input), human(a.tokens.output)),
            a.failures.to_string(),
            a.activity.clone(),
        ])
    });
    let widths = [
        Constraint::Length(30),
        Constraint::Length(20),
        Constraint::Length(6),
        Constraint::Length(13),
        Constraint::Length(4),
        Constraint::Min(20),
    ];
    frame.render_widget(
        Table::new(rows, widths)
            .header(Row::new(vec!["job", "rung", "time", "tokens", "fail", "doing"]).bold())
            .block(Block::bordered().title(format!(" agents ({}) ", view.agents.len()))),
        area,
    );
}

fn draw_rungs(frame: &mut Frame, area: Rect, view: &View) {
    let rows = view.rungs.iter().map(|r| {
        Row::new(vec![
            r.rung.to_string(),
            r.label.clone(),
            r.attempts.to_string(),
            r.successes.to_string(),
            format!(
                "{} in ({} cached) / {} out",
                human(r.tokens.input),
                human(r.tokens.cached_input),
                human(r.tokens.output)
            ),
        ])
    });
    let widths = [
        Constraint::Length(4),
        Constraint::Length(28),
        Constraint::Length(9),
        Constraint::Length(9),
        Constraint::Min(20),
    ];
    frame.render_widget(
        Table::new(rows, widths)
            .header(Row::new(vec!["rung", "model", "attempts", "results", "tokens"]).bold())
            .block(Block::bordered().title(" model ladder ")),
        area,
    );
}
