//! Terminal UI for pext. All state lives in [`Tui`] (a plain view-model that
//! is easy to test); drawing is a pure function of that state, so the whole
//! thing can be exercised with ratatui's `TestBackend`.

#![forbid(unsafe_code)]
#![allow(clippy::result_large_err)]

use pext_core::money::format_grouped;
use pext_engine::{Actor, CallOpts, Engine};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Row, Table, TableState, Tabs, Wrap};
use ratatui::Frame;
use serde_json::{Map, Value};
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tab {
    Overview,
    Transactions,
    Budgets,
    Goals,
    NetWorth,
}

impl Tab {
    pub const ALL: [Tab; 5] = [
        Tab::Overview,
        Tab::Transactions,
        Tab::Budgets,
        Tab::Goals,
        Tab::NetWorth,
    ];
    pub fn title(self) -> &'static str {
        match self {
            Tab::Overview => "1 Overview",
            Tab::Transactions => "2 Transactions",
            Tab::Budgets => "3 Budgets",
            Tab::Goals => "4 Goals",
            Tab::NetWorth => "5 Net worth",
        }
    }
    fn index(self) -> usize {
        Tab::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }
}

#[derive(Default)]
pub struct Data {
    pub networth: Value,
    pub summary: Value,
    pub accounts: Value,
    pub budgets: Value,
    pub goals: Value,
    pub alerts: Value,
    pub txns: Value,
}

pub struct Tui {
    engine: Engine,
    actor: Actor,
    pub tab: Tab,
    /// Selected row in the transactions table.
    pub selected: usize,
    pub private: bool,
    pub help: bool,
    pub quit: bool,
    pub data: Data,
    pub errors: Vec<String>,
}

/// Strip control characters (terminal escape sequences in a payee name must
/// never reach the terminal) and cap the length.
pub fn clean(s: &str, max: usize) -> String {
    let mut out: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if out.chars().count() > max {
        out = out.chars().take(max.saturating_sub(1)).collect::<String>() + "…";
    }
    out
}

pub fn bar(pct: i64, width: usize) -> String {
    let pct = pct.clamp(0, 100) as usize;
    let filled = pct * width / 100;
    "█".repeat(filled) + &"░".repeat(width - filled)
}

fn i(v: &Value, k: &str) -> i64 {
    v.get(k).and_then(|x| x.as_i64()).unwrap_or(0)
}
fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("")
}
fn arr(v: &Value) -> &[Value] {
    v.as_array().map(|a| a.as_slice()).unwrap_or(&[])
}

impl Tui {
    pub fn new(engine: Engine, actor: Actor) -> Tui {
        let mut t = Tui {
            engine,
            actor,
            tab: Tab::Overview,
            selected: 0,
            private: false,
            help: false,
            quit: false,
            data: Data::default(),
            errors: vec![],
        };
        t.refresh();
        t
    }

    fn fetch(&mut self, op: &str, args: Map<String, Value>) -> Value {
        match self
            .engine
            .call(op, args, &CallOpts::default(), &self.actor)
        {
            Ok(o) => o.data,
            Err(e) => {
                self.errors.push(format!("{op}: {}", e.message));
                Value::Null
            }
        }
    }

    pub fn refresh(&mut self) {
        self.errors.clear();
        let mut limit = Map::new();
        limit.insert("limit".into(), Value::from(200));
        self.data = Data {
            networth: self.fetch("networth", Map::new()),
            summary: self.fetch("report.summary", Map::new()),
            accounts: self.fetch("acct.list", Map::new()),
            budgets: self.fetch("budget.status", Map::new()),
            goals: self.fetch("goal.status", Map::new()),
            alerts: self.fetch("alert.check", Map::new()),
            txns: self.fetch("txn.list", limit),
        };
        let n = arr(&self.data.txns).len();
        if self.selected >= n {
            self.selected = n.saturating_sub(1);
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if self.help {
            // any key closes help
            self.help = false;
            return;
        }
        let n = arr(&self.data.txns).len();
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('?') | KeyCode::F(1) => self.help = true,
            KeyCode::Char('r') => self.refresh(),
            KeyCode::Char('p') => self.private = !self.private,
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => self.step_tab(1),
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => self.step_tab(-1),
            KeyCode::Char(c @ '1'..='5') => self.tab = Tab::ALL[(c as usize) - ('1' as usize)],
            KeyCode::Down | KeyCode::Char('j') => {
                if self.tab == Tab::Transactions && self.selected + 1 < n {
                    self.selected += 1;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::PageDown => self.selected = (self.selected + 10).min(n.saturating_sub(1)),
            KeyCode::PageUp => self.selected = self.selected.saturating_sub(10),
            KeyCode::Home | KeyCode::Char('g') => self.selected = 0,
            KeyCode::End | KeyCode::Char('G') => self.selected = n.saturating_sub(1),
            _ => {}
        }
    }

    fn step_tab(&mut self, d: isize) {
        let n = Tab::ALL.len() as isize;
        let next = (self.tab.index() as isize + d).rem_euclid(n) as usize;
        self.tab = Tab::ALL[next];
    }

    fn money(&self, minor: i64, cur: &str) -> String {
        if self.private {
            "••••".to_string()
        } else {
            format_grouped(minor, cur)
        }
    }

    fn money_style(minor: i64) -> Style {
        if minor < 0 {
            Style::default().fg(Color::Red)
        } else {
            Style::default().fg(Color::Green)
        }
    }

    // ---- drawing ----------------------------------------------------------------

    pub fn draw(&self, f: &mut Frame) {
        let area = f.area();
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(3),
                Constraint::Length(1),
            ])
            .split(area);
        let titles: Vec<Line> = Tab::ALL.iter().map(|t| Line::from(t.title())).collect();
        let asof = s(&self.data.networth, "date");
        let title = if asof.is_empty() {
            " pext ".to_string()
        } else {
            format!(" pext · {asof} ")
        };
        f.render_widget(
            Tabs::new(titles)
                .select(self.tab.index())
                .block(Block::default().borders(Borders::ALL).title(title))
                .highlight_style(
                    Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED),
                ),
            rows[0],
        );
        match self.tab {
            Tab::Overview => self.draw_overview(f, rows[1]),
            Tab::Transactions => self.draw_txns(f, rows[1]),
            Tab::Budgets => self.draw_budgets(f, rows[1]),
            Tab::Goals => self.draw_goals(f, rows[1]),
            Tab::NetWorth => self.draw_networth(f, rows[1]),
        }
        let status = if let Some(e) = self.errors.first() {
            format!(" ! {e}")
        } else {
            let p = if self.private {
                " · amounts hidden"
            } else {
                ""
            };
            format!(" q quit · ? help · tab/1-5 switch · r refresh · p privacy{p}")
        };
        let style = if self.errors.is_empty() {
            Style::default().add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(Color::Red)
        };
        f.render_widget(
            Paragraph::new(clean(&status, area.width as usize)).style(style),
            rows[2],
        );
        if self.help {
            self.draw_help(f, area);
        }
    }

    fn block(title: impl Into<String>) -> Block<'static> {
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" {} ", title.into()))
    }

    fn draw_overview(&self, f: &mut Frame, area: Rect) {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);
        let left = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(6), Constraint::Min(4)])
            .split(cols[0]);
        let right = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(cols[1]);

        // Net worth
        let nw = &self.data.networth;
        let cur = s(nw, "currency");
        let mut lines = vec![
            Line::from(Span::styled(
                format!("{} {cur}", self.money(i(nw, "net_worth_minor"), cur)),
                Self::money_style(i(nw, "net_worth_minor")).add_modifier(Modifier::BOLD),
            )),
            Line::from(format!(
                "Assets {}  ·  Liabilities {}",
                self.money(i(nw, "assets_minor"), cur),
                self.money(i(nw, "liabilities_minor"), cur)
            )),
        ];
        if nw.is_null() {
            lines = vec![Line::from("unavailable")];
        }
        f.render_widget(
            Paragraph::new(lines).block(Self::block("Net worth")),
            left[0],
        );

        // This month
        let sm = &self.data.summary;
        let cur = s(sm, "currency");
        let mut lines = vec![Line::from(format!(
            "In {}  ·  Out {}  ·  Net {}",
            self.money(i(sm, "income_minor"), cur),
            self.money(i(sm, "expense_minor"), cur),
            self.money(i(sm, "net_minor"), cur)
        ))];
        let width = (left[1].width as usize).saturating_sub(40).clamp(6, 30);
        for c in arr(&sm["top_categories"]).iter().take(6) {
            lines.push(Line::from(format!(
                "{:<18} {} {:>12}",
                clean(s(c, "name"), 18),
                bar(i(c, "share_pct"), width),
                self.money(i(c, "amount_minor"), cur)
            )));
        }
        if sm.is_null() {
            lines = vec![Line::from("unavailable")];
        }
        f.render_widget(
            Paragraph::new(lines).block(Self::block("This month")),
            left[1],
        );

        // Alerts
        let mut lines: Vec<Line> = arr(&self.data.alerts)
            .iter()
            .take(8)
            .map(|a| {
                let color = match s(a, "severity") {
                    "critical" => Color::Red,
                    "warning" => Color::Yellow,
                    _ => Color::Gray,
                };
                Line::from(vec![
                    Span::styled(
                        format!("[{}] ", s(a, "severity")),
                        Style::default().fg(color),
                    ),
                    Span::raw(clean(s(a, "message"), 200)),
                ])
            })
            .collect();
        if lines.is_empty() {
            lines.push(Line::from("Nothing needs attention."));
        }
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: true })
                .block(Self::block("Needs attention")),
            right[0],
        );

        // Accounts
        let lines: Vec<Line> = arr(&self.data.accounts)
            .iter()
            .filter(|a| a["archived"] != Value::Bool(true))
            .map(|a| {
                let bal = i(a, "balance_minor");
                Line::from(vec![
                    Span::raw(format!("{:<24}", clean(s(a, "name"), 24))),
                    Span::styled(
                        format!(
                            "{:>16} {}",
                            self.money(bal, s(a, "currency")),
                            s(a, "currency")
                        ),
                        Self::money_style(bal),
                    ),
                ])
            })
            .collect();
        f.render_widget(
            Paragraph::new(lines).block(Self::block("Accounts")),
            right[1],
        );
    }

    fn draw_txns(&self, f: &mut Frame, area: Rect) {
        let list = arr(&self.data.txns);
        let rows: Vec<Row> = list
            .iter()
            .map(|t| {
                let amt = i(t, "amount_minor");
                Row::new(vec![
                    ratatui::widgets::Cell::from(s(t, "date").to_string()),
                    ratatui::widgets::Cell::from(clean(s(t, "payee"), 28)),
                    ratatui::widgets::Cell::from(clean(s(t, "category"), 22)),
                    ratatui::widgets::Cell::from(clean(s(t, "account"), 18)),
                    ratatui::widgets::Cell::from(
                        Line::from(self.money(amt, s(t, "currency"))).alignment(Alignment::Right),
                    )
                    .style(Self::money_style(amt)),
                ])
            })
            .collect();
        let table = Table::new(
            rows,
            [
                Constraint::Length(11),
                Constraint::Min(16),
                Constraint::Length(22),
                Constraint::Length(18),
                Constraint::Length(16),
            ],
        )
        .header(
            Row::new(vec!["Date", "Payee", "Category", "Account", "Amount"])
                .style(Style::default().add_modifier(Modifier::BOLD)),
        )
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .block(Self::block(format!("Transactions ({})", list.len())));
        let mut state = TableState::default();
        if !list.is_empty() {
            state.select(Some(self.selected));
        }
        f.render_stateful_widget(table, area, &mut state);
    }

    fn draw_budgets(&self, f: &mut Frame, area: Rect) {
        let b = &self.data.budgets;
        let cur = s(b, "currency");
        let mut lines = vec![];
        for x in arr(&b["budgets"]) {
            let color = match s(x, "state") {
                "over" => Color::Red,
                "warning" => Color::Yellow,
                _ => Color::Green,
            };
            lines.push(Line::from(vec![
                Span::raw(format!("{:<22}", clean(s(x, "category"), 22))),
                Span::styled(bar(i(x, "pct_used"), 24), Style::default().fg(color)),
                Span::raw(format!(
                    " {:>3}%  {} of {}  ({} left)",
                    i(x, "pct_used"),
                    self.money(i(x, "spent_minor"), cur),
                    self.money(i(x, "available_minor"), cur),
                    self.money(i(x, "remaining_minor"), cur)
                )),
            ]));
        }
        if lines.is_empty() {
            lines.push(Line::from(
                "No budgets set. Try `pext budget set --category food --amount 20000`.",
            ));
        }
        f.render_widget(
            Paragraph::new(lines).block(Self::block("Budgets (this month)")),
            area,
        );
    }

    fn draw_goals(&self, f: &mut Frame, area: Rect) {
        let mut lines = vec![];
        for g in arr(&self.data.goals)
            .iter()
            .filter(|g| g["archived"] != Value::Bool(true))
        {
            let cur = s(g, "currency");
            lines.push(Line::from(vec![
                Span::raw(format!("{:<22}", clean(s(g, "name"), 22))),
                Span::raw(bar(i(g, "pct"), 24)),
                Span::raw(format!(
                    " {:>3}%  {} of {}",
                    i(g, "pct"),
                    self.money(i(g, "saved_minor"), cur),
                    self.money(i(g, "target_minor"), cur)
                )),
            ]));
            let when = g
                .get("target_date")
                .and_then(|v| v.as_str())
                .unwrap_or("no date");
            lines.push(Line::from(Span::styled(
                format!("    by {when} · {}", s(g, "state")),
                Style::default().add_modifier(Modifier::DIM),
            )));
        }
        if lines.is_empty() {
            lines.push(Line::from(
                "No goals yet. Try `pext goal add --name Trip --target 100000`.",
            ));
        }
        f.render_widget(Paragraph::new(lines).block(Self::block("Goals")), area);
    }

    fn draw_networth(&self, f: &mut Frame, area: Rect) {
        let nw = &self.data.networth;
        let base = s(nw, "currency");
        let rows: Vec<Row> = arr(&nw["items"])
            .iter()
            .map(|it| {
                let v = i(it, "value_base_minor");
                Row::new(vec![
                    ratatui::widgets::Cell::from(clean(s(it, "group"), 14)),
                    ratatui::widgets::Cell::from(clean(s(it, "name"), 30)),
                    ratatui::widgets::Cell::from(s(it, "kind").to_string()),
                    ratatui::widgets::Cell::from(
                        Line::from(self.money(v, base)).alignment(Alignment::Right),
                    )
                    .style(Self::money_style(v)),
                ])
            })
            .collect();
        let title = format!(
            "Net worth {} (assets {} · liabilities {})",
            self.money(i(nw, "net_worth_minor"), base),
            self.money(i(nw, "assets_minor"), base),
            self.money(i(nw, "liabilities_minor"), base)
        );
        let table = Table::new(
            rows,
            [
                Constraint::Length(14),
                Constraint::Min(20),
                Constraint::Length(14),
                Constraint::Length(18),
            ],
        )
        .header(
            Row::new(vec!["Group", "Item", "Kind", "Value"])
                .style(Style::default().add_modifier(Modifier::BOLD)),
        )
        .block(Self::block(title));
        f.render_widget(table, area);
    }

    fn draw_help(&self, f: &mut Frame, area: Rect) {
        let w = 52.min(area.width);
        let h = 13.min(area.height);
        let r = Rect::new(
            area.x + (area.width - w) / 2,
            area.y + (area.height - h) / 2,
            w,
            h,
        );
        f.render_widget(Clear, r);
        let text = vec![
            Line::from("Tab / ← →, h l   switch view"),
            Line::from("1-5              jump to a view"),
            Line::from("↑ ↓, j k         move in transactions"),
            Line::from("PgUp PgDn g G    jump"),
            Line::from("r                reload data"),
            Line::from("p                hide / show amounts"),
            Line::from("?                this help"),
            Line::from("q, Esc, Ctrl-C   quit"),
            Line::from(""),
            Line::from("The TUI only reads; use the CLI to change data."),
        ];
        f.render_widget(
            Paragraph::new(text)
                .block(Self::block("Keys"))
                .wrap(Wrap { trim: false }),
            r,
        );
    }
}

/// Run the interactive terminal UI until the user quits.
pub fn run_terminal(engine: Engine, actor: Actor) -> std::io::Result<()> {
    let mut tui = Tui::new(engine, actor);
    let mut terminal = ratatui::init();
    let result = (|| -> std::io::Result<()> {
        while !tui.quit {
            terminal.draw(|f| tui.draw(f))?;
            if event::poll(Duration::from_millis(250))? {
                if let Event::Key(k) = event::read()? {
                    tui.on_key(k);
                }
            }
        }
        Ok(())
    })();
    ratatui::restore();
    result
}
