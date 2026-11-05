use pext_engine::{Actor, CallOpts, Engine, EngineConfig};
use pext_tui::{bar, clean, Tab, Tui};
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use ratatui::Terminal;
use serde_json::{json, Map, Value};
use tempfile::TempDir;

struct Fx {
    _dir: TempDir,
    cfg: EngineConfig,
}

impl Fx {
    fn new() -> Fx {
        let dir = TempDir::new().unwrap();
        let mut cfg = EngineConfig::new(dir.path().join("t.db"));
        cfg.today = chrono::NaiveDate::from_ymd_opt(2026, 11, 15);
        cfg.kdf = pext_engine::crypto::KdfParams::insecure_fast();
        let fx = Fx { _dir: dir, cfg };
        fx.call("init", json!({}));
        fx
    }
    fn engine(&self) -> Engine {
        Engine::new(self.cfg.clone())
    }
    fn call(&self, op: &str, args: Value) -> Value {
        self.engine()
            .call(
                op,
                args.as_object().cloned().unwrap_or_default(),
                &CallOpts::default(),
                &Actor::owner(),
            )
            .unwrap_or_else(|e| panic!("{op}: {}", e.message))
            .data
    }
    fn seed(&self) {
        self.call(
            "acct.add",
            json!({"name": "HDFC Bank", "kind": "checking", "opening": "250000"}),
        );
        self.call(
            "acct.add",
            json!({"name": "Credit Card", "kind": "credit_card", "opening": "0"}),
        );
        self.call("txn.add", json!({"account": "HDFC Bank", "amount": "85000", "category": "salary", "payee": "Employer", "date": "2026-11-01"}));
        self.call("txn.add", json!({"account": "HDFC Bank", "amount": "-12000", "category": "food.groceries", "payee": "Big Bazaar", "date": "2026-11-03"}));
        self.call("txn.add", json!({"account": "Credit Card", "amount": "-4500", "category": "food.dining", "payee": "Cafe", "date": "2026-11-04"}));
        self.call("budget.set", json!({"category": "food", "amount": "20000"}));
        self.call(
            "goal.add",
            json!({"name": "Goa trip", "target": "100000", "by": "2027-06-01"}),
        );
    }
    fn tui(&self) -> Tui {
        Tui::new(self.engine(), Actor::owner())
    }
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}
fn ch(c: char) -> KeyEvent {
    key(KeyCode::Char(c))
}

fn screen_sized(t: &Tui, w: u16, h: u16) -> String {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| t.draw(f)).unwrap();
    let buf = term.backend().buffer().clone();
    let mut out = String::new();
    for row in buf.content().chunks(w as usize) {
        for c in row {
            out.push_str(c.symbol());
        }
        out.push('\n');
    }
    out
}
fn screen(t: &Tui) -> String {
    screen_sized(t, 120, 32)
}

#[test]
fn overview_shows_the_essentials() {
    let fx = Fx::new();
    fx.seed();
    let t = fx.tui();
    let s = screen(&t);
    for want in [
        "pext",
        "Net worth",
        "3,18,500.00 INR",
        "This month",
        "food.groceries",
        "Needs attention",
        "food has used",
        "Accounts",
        "HDFC Bank",
        "Credit Card",
        "-4,500.00 INR",
    ] {
        assert!(s.contains(want), "missing {want:?} in:\n{s}");
    }
    assert!(s.contains("1 Overview") && s.contains("5 Net worth"));
}

#[test]
fn views_switch_with_keys_and_wrap() {
    let fx = Fx::new();
    fx.seed();
    let mut t = fx.tui();
    assert_eq!(t.tab, Tab::Overview);
    t.on_key(ch('2'));
    assert_eq!(t.tab, Tab::Transactions);
    let s = screen(&t);
    assert!(
        s.contains("Transactions (3)") && s.contains("Big Bazaar") && s.contains("-12,000.00"),
        "{s}"
    );
    t.on_key(ch('3'));
    let s = screen(&t);
    assert!(
        s.contains("Budgets") && s.contains("82%") && s.contains("food"),
        "{s}"
    );
    t.on_key(ch('4'));
    let s = screen(&t);
    assert!(
        s.contains("Goa trip") && s.contains("1,00,000.00") && s.contains("by 2027-06-01"),
        "{s}"
    );
    t.on_key(ch('5'));
    let s = screen(&t);
    assert!(s.contains("HDFC Bank") && s.contains("checking"), "{s}");
    t.on_key(key(KeyCode::Tab));
    assert_eq!(t.tab, Tab::Overview, "tab wraps forward");
    t.on_key(key(KeyCode::BackTab));
    assert_eq!(t.tab, Tab::NetWorth, "backtab wraps backward");
    t.on_key(key(KeyCode::Left));
    assert_eq!(t.tab, Tab::Goals);
    t.on_key(key(KeyCode::Right));
    assert_eq!(t.tab, Tab::NetWorth);
    t.on_key(ch('9'));
    assert_eq!(t.tab, Tab::NetWorth, "unknown keys do nothing");
}

#[test]
fn selection_moves_and_clamps() {
    let fx = Fx::new();
    fx.seed();
    let mut t = fx.tui();
    t.on_key(ch('2'));
    assert_eq!(t.selected, 0);
    t.on_key(ch('j'));
    t.on_key(key(KeyCode::Down));
    assert_eq!(t.selected, 2);
    t.on_key(key(KeyCode::Down));
    assert_eq!(t.selected, 2, "cannot go past the last row");
    t.on_key(ch('k'));
    assert_eq!(t.selected, 1);
    t.on_key(ch('g'));
    assert_eq!(t.selected, 0);
    t.on_key(key(KeyCode::Up));
    assert_eq!(t.selected, 0);
    t.on_key(ch('G'));
    assert_eq!(t.selected, 2);
    t.on_key(key(KeyCode::PageUp));
    assert_eq!(t.selected, 0);
    t.on_key(key(KeyCode::PageDown));
    assert_eq!(t.selected, 2);
    // selection is not moved by down-arrow on other tabs
    t.on_key(ch('1'));
    t.on_key(ch('g'));
    t.on_key(key(KeyCode::Down));
    assert_eq!(t.selected, 0);
}

#[test]
fn privacy_mode_hides_every_amount() {
    let fx = Fx::new();
    fx.seed();
    let mut t = fx.tui();
    t.on_key(ch('p'));
    assert!(t.private);
    for tab in ['1', '2', '3', '4', '5'] {
        t.on_key(ch(tab));
        let s = screen(&t);
        assert!(s.contains("amounts hidden"), "tab {tab}");
        for secret in [
            "3,18,500", "3,23,000", "12,000", "4,500", "20,000", "85,000", "16,500",
        ] {
            assert!(!s.contains(secret), "tab {tab} leaks {secret}:\n{s}");
        }
    }
    t.on_key(ch('p'));
    t.on_key(ch('1'));
    assert!(screen(&t).contains("3,18,500.00 INR"));
}

#[test]
fn terminal_escape_sequences_in_data_never_reach_the_screen() {
    // The engine refuses control characters on write; this is defence in depth
    // for data that arrives some other way (imports from old versions, sync, hand edits).
    let fx = Fx::new();
    fx.seed();
    let mut t = fx.tui();
    t.data.txns = json!([{
        "id": 1, "date": "2026-11-10", "account": "HDFC Bank\u{1b}[2J", "amount_minor": -100, "currency": "INR",
        "payee": "\u{1b}[31mEVIL\u{1b}]0;pwned\u{7}\u{9b}2J", "category": "x\ry", "memo": ""
    }]);
    t.data.alerts = json!([{"severity": "warning\u{1b}", "message": "bad \u{1b}[1m message\u{8}"}]);
    for tab in ['1', '2'] {
        t.on_key(ch(tab));
        let s = screen(&t);
        for c in s.chars() {
            assert!(
                !c.is_control() || c == '\n',
                "control character {:?} reached the screen:\n{s}",
                c
            );
        }
    }
    t.on_key(ch('2'));
    assert!(screen(&t).contains("EVIL"));
    assert_eq!(clean("a\u{1b}[2Jb\tc\nd", 100), "a [2Jb c d");
}

#[test]
fn help_overlay_and_quit_keys() {
    let fx = Fx::new();
    fx.seed();
    let mut t = fx.tui();
    t.on_key(ch('?'));
    assert!(t.help);
    assert!(screen(&t).contains("hide / show amounts"));
    t.on_key(ch('q'));
    assert!(!t.help && !t.quit, "the first key only closes help");
    t.on_key(ch('q'));
    assert!(t.quit);
    for k in [
        key(KeyCode::Esc),
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
    ] {
        let mut t = fx.tui();
        t.on_key(k);
        assert!(t.quit);
    }
    // key release events are ignored (Windows terminals send them)
    let mut t = fx.tui();
    t.on_key(KeyEvent {
        code: KeyCode::Char('q'),
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Release,
        state: KeyEventState::NONE,
    });
    assert!(!t.quit);
}

#[test]
fn refresh_picks_up_changes_made_elsewhere() {
    let fx = Fx::new();
    fx.seed();
    let mut t = fx.tui();
    t.on_key(ch('2'));
    assert!(screen(&t).contains("Transactions (3)"));
    fx.call(
        "txn.add",
        json!({"account": "HDFC Bank", "amount": "-77", "payee": "NewShop", "date": "2026-11-10"}),
    );
    assert!(
        screen(&t).contains("Transactions (3)"),
        "no live updates until asked"
    );
    t.on_key(ch('r'));
    let s = screen(&t);
    assert!(
        s.contains("Transactions (4)") && s.contains("NewShop"),
        "{s}"
    );
    // selection stays valid when rows disappear
    t.on_key(ch('G'));
    assert_eq!(t.selected, 3);
    fx.call("undo", json!({}));
    t.on_key(ch('r'));
    assert_eq!(t.selected, 2);
}

#[test]
fn redacting_actors_see_masked_text() {
    let fx = Fx::new();
    fx.seed();
    let secret = fx.call(
        "token.create",
        json!({"name": "view", "role": "read", "redact": true}),
    )["token"]
        .as_str()
        .unwrap()
        .to_string();
    let actor = fx.engine().authenticate(&secret).unwrap();
    let mut t = Tui::new(fx.engine(), actor);
    t.on_key(ch('2'));
    let s = screen(&t);
    assert!(!s.contains("Big Bazaar") && !s.contains("Employer"), "{s}");
    assert!(s.contains("12,000"), "amounts are still shown");
}

#[test]
fn denied_operations_show_in_the_status_bar_but_do_not_break_the_ui() {
    let fx = Fx::new();
    fx.seed();
    let secret = fx.call(
        "token.create",
        json!({"name": "scoped", "role": "read", "account": ["HDFC Bank"]}),
    )["token"]
        .as_str()
        .unwrap()
        .to_string();
    let actor = fx.engine().authenticate(&secret).unwrap();
    let t = Tui::new(fx.engine(), actor);
    assert!(!t.errors.is_empty());
    let s = screen(&t);
    assert!(s.contains(" ! "), "status bar shows the problem:\n{s}");
    assert!(s.contains("Accounts"));
}

#[test]
fn empty_ledger_and_tiny_terminals_do_not_panic() {
    let fx = Fx::new();
    let mut t = fx.tui();
    for tab in ['1', '2', '3', '4', '5'] {
        t.on_key(ch(tab));
        let s = screen(&t);
        assert!(!s.is_empty());
    }
    t.on_key(ch('3'));
    assert!(screen(&t).contains("No budgets set"));
    t.on_key(ch('4'));
    assert!(screen(&t).contains("No goals yet"));
    fx.seed();
    t.on_key(ch('r'));
    t.on_key(ch('?'));
    for (w, h) in [
        (1, 1),
        (2, 2),
        (10, 3),
        (20, 5),
        (40, 8),
        (60, 12),
        (200, 60),
    ] {
        for tab in ['1', '2', '3', '4', '5'] {
            t.on_key(ch('?')); // toggle help on/off over the sizes
            t.on_key(ch(tab));
            let _ = screen_sized(&t, w, h);
        }
    }
}

#[test]
fn bar_and_clean_helpers() {
    assert_eq!(bar(0, 10), "░░░░░░░░░░");
    assert_eq!(bar(50, 10), "█████░░░░░");
    assert_eq!(bar(100, 10), "██████████");
    assert_eq!(bar(250, 10), "██████████", "over 100% is clamped");
    assert_eq!(bar(-5, 4), "░░░░");
    assert_eq!(bar(33, 0), "");
    assert_eq!(clean("short", 10), "short");
    assert_eq!(clean("abcdefghij", 5), "abcd…");
    assert_eq!(clean("₹₹₹₹₹₹", 4), "₹₹₹…");
    let _: Map<String, Value> = Map::new();
}
