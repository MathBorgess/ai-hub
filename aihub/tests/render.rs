//! Rendering tests using ratatui's TestBackend.

use aihub_core::{
    HarnessId, LaneKind, MergeStrategy, Mode, QuotaLane, QuotaSnapshot, QuotaStatus, QuotaWindow,
    SlotId, TaskTier, WindowKind,
};
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use ratatui::Terminal;
use std::path::PathBuf;

use aihub::colors::{quota_color, QUOTA_CRIT_PCT, QUOTA_WARN_PCT};
use aihub::state::{App, RecommendationState};
use aihub::ui;

fn buffer_to_text(buf: &Buffer) -> String {
    let mut text = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            text.push_str(buf[(x, y)].symbol());
        }
        text.push('\n');
    }
    text
}

#[test]
fn test_quota_threshold_colors() {
    assert_eq!(quota_color(0.0), Color::Green);
    assert_eq!(quota_color(QUOTA_WARN_PCT - 1.0), Color::Green);
    assert_eq!(quota_color(QUOTA_WARN_PCT), Color::Yellow);
    assert_eq!(quota_color(QUOTA_CRIT_PCT - 1.0), Color::Yellow);
    assert_eq!(quota_color(QUOTA_CRIT_PCT), Color::Red);
    assert_eq!(quota_color(100.0), Color::Red);
}

#[test]
fn test_render_header_assisted_and_autonomous() {
    let backend = TestBackend::new(120, 10);
    let mut terminal = Terminal::new(backend).unwrap();

    let mut app = App::new(PathBuf::from("/test/repo"));
    app.mode = Mode::Assisted;
    app.harness = HarnessId::Antigravity;
    app.branch = "session/test-worktree".to_string();

    terminal
        .draw(|f| {
            ui::header::render_header(&app, Rect::new(0, 0, 120, 1), f.buffer_mut());
        })
        .unwrap();

    let text = buffer_to_text(terminal.backend().buffer());
    assert!(
        text.contains("[ASSISTIDO]"),
        "Header should contain [ASSISTIDO]"
    );
    assert!(
        text.contains("[agy]"),
        "Header should contain harness [agy]"
    );
    assert!(
        text.contains("(session/test-worktree)"),
        "Header should contain branch name"
    );

    // Switch to Autonomous mode
    app.mode = Mode::Autonomous;
    app.harness = HarnessId::ClaudeCode;
    terminal
        .draw(|f| {
            ui::header::render_header(&app, Rect::new(0, 0, 120, 1), f.buffer_mut());
        })
        .unwrap();

    let text = buffer_to_text(terminal.backend().buffer());
    assert!(
        text.contains("[AUTÔNOMO]"),
        "Header should contain [AUTÔNOMO]"
    );
    assert!(text.contains("[claude]"), "Header should contain [claude]");
}

#[test]
fn test_render_header_quota_bars_multi_window_and_lanes() {
    let backend = TestBackend::new(140, 10);
    let mut terminal = Terminal::new(backend).unwrap();

    let mut app = App::new(PathBuf::from("/test/repo"));
    app.snapshots = vec![
        QuotaSnapshot {
            slot: SlotId::new(HarnessId::ClaudeCode, "default"),
            status: QuotaStatus::Ok,
            source: aihub_core::QuotaSource::Vendor,
            estimated: false,
            note: None,
            windows: vec![
                QuotaWindow::new(WindowKind::FiveHour, 35.0, Some(3600), Some(18000)),
                QuotaWindow::new(WindowKind::SevenDay, 75.0, Some(72000), Some(604800)),
            ],
            lanes: vec![],
        },
        QuotaSnapshot {
            slot: SlotId::new(HarnessId::CursorAgent, "pro"),
            status: QuotaStatus::Low,
            source: aihub_core::QuotaSource::Vendor,
            estimated: false,
            note: None,
            windows: vec![],
            lanes: vec![
                QuotaLane {
                    name: "cursor-models".to_string(),
                    kind: LaneKind::Own,
                    windows: vec![QuotaWindow::new(WindowKind::Cycle, 20.0, None, None)],
                },
                QuotaLane {
                    name: "other-models".to_string(),
                    kind: LaneKind::Frontier,
                    windows: vec![QuotaWindow::new(WindowKind::Cycle, 95.0, None, None)],
                },
            ],
        },
    ];

    terminal
        .draw(|f| {
            ui::header::render_header(&app, Rect::new(0, 0, 140, 1), f.buffer_mut());
        })
        .unwrap();

    let text = buffer_to_text(terminal.backend().buffer());
    assert!(text.contains("claude:"));
    assert!(text.contains("5h:35%"));
    assert!(text.contains("7d:75%"));
    assert!(text.contains("cursor-agent:"));
    assert!(text.contains("cursor-models:20%"));
    assert!(text.contains("other-models:95%"));
    assert!(
        text.contains('█'),
        "Header should contain filled gauge blocks"
    );
    assert!(
        text.contains('░'),
        "Header should contain empty gauge blocks"
    );

    // Check specific cell colors
    let buf = terminal.backend().buffer();
    // Find where "other-models:95%" is rendered and verify its fg is Red
    let mut found_red = false;
    let mut found_green = false;
    let mut found_yellow = false;
    for x in 0..140 {
        let cell = &buf[(x, 0)];
        if cell.style().fg == Some(Color::Red) {
            found_red = true;
        }
        if cell.style().fg == Some(Color::Yellow) {
            found_yellow = true;
        }
        if cell.style().fg == Some(Color::Green) {
            found_green = true;
        }
    }
    assert!(found_green, "35% should be colored Green");
    assert!(found_yellow, "75% should be colored Yellow");
    assert!(found_red, "95% should be colored Red");
}

#[test]
fn test_render_recommendation_banner_and_prefix_hint() {
    let backend = TestBackend::new(140, 5);
    let mut terminal = Terminal::new(backend).unwrap();

    let mut app = App::new(PathBuf::from("/test/repo"));
    app.mode = Mode::Assisted;
    app.recommendation = Some(RecommendationState::Recommended {
        tier: TaskTier::Mechanical,
        harness: HarnessId::Antigravity,
        lane: Some("gemini".to_string()),
        model: None,
        holds_until_s: None,
        confidence: 0.92,
        reason: "Refactor mecánico rápido".to_string(),
    });

    terminal
        .draw(|f| {
            ui::footer::render_footer(&app, Rect::new(0, 0, 140, 3), f.buffer_mut());
        })
        .unwrap();

    let text = buffer_to_text(terminal.backend().buffer());
    assert!(text.contains("RECOMENDAÇÃO"));
    assert!(text.contains("Trocar para agy"));
    assert!(text.contains("Refactor mecánico rápido"));
    assert!(text.contains("92%"));
    assert!(text.contains("Pressione ^] seguido de Enter para aceitar"));
    assert!(text.contains("prefixo:"));
    assert!(text.contains("[p|:] paleta"));

    // Test with prefix active
    app.prefix_active = true;
    terminal
        .draw(|f| {
            ui::footer::render_footer(&app, Rect::new(0, 0, 140, 3), f.buffer_mut());
        })
        .unwrap();

    let text2 = buffer_to_text(terminal.backend().buffer());
    assert!(text2.contains("PREFIXO ATIVO (^])"));
}

#[test]
fn test_render_recommendation_banner_held_until() {
    let backend = TestBackend::new(140, 5);
    let mut terminal = Terminal::new(backend).unwrap();

    let mut app = App::new(PathBuf::from("/test/repo"));
    app.mode = Mode::Assisted;
    app.now_override = Some(50000);
    app.recommendation = Some(RecommendationState::Recommended {
        tier: TaskTier::Mechanical,
        harness: HarnessId::Antigravity,
        lane: Some("gemini".to_string()),
        model: None,
        holds_until_s: Some(50400),
        confidence: 0.92,
        reason: "Refactor mecánico rápido".to_string(),
    });

    terminal
        .draw(|f| {
            ui::footer::render_footer(&app, Rect::new(0, 0, 140, 3), f.buffer_mut());
        })
        .unwrap();

    let text = buffer_to_text(terminal.backend().buffer());
    assert!(text.contains("RECOMENDAÇÃO"));
    assert!(text.contains("Trocar para agy"));
    assert!(text.contains("held until 14:00"));
}

#[test]
fn test_render_palette() {
    let backend = TestBackend::new(90, 15);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal
        .draw(|f| {
            ui::palette::render_palette("/switch agy", Rect::new(0, 0, 90, 15), f.buffer_mut());
        })
        .unwrap();

    let text = buffer_to_text(terminal.backend().buffer());
    assert!(text.contains("Paleta de Comandos"));
    assert!(text.contains(": /switch agy"));
    assert!(text.contains("/switch <harness>"));
    assert!(text.contains("/merge"));
    assert!(text.contains("/quota"));
    assert!(text.contains("/mode"));
    assert!(text.contains("/detach"));
    assert!(text.contains("[Tab] autocompletar"));
}

#[test]
fn test_render_quota_table() {
    let backend = TestBackend::new(100, 20);
    let mut terminal = Terminal::new(backend).unwrap();

    let snapshots = vec![QuotaSnapshot {
        slot: SlotId::new(HarnessId::Antigravity, "work"),
        status: QuotaStatus::Ok,
        source: aihub_core::QuotaSource::Vendor,
        estimated: false,
        note: None,
        windows: vec![QuotaWindow::new(
            WindowKind::FiveHour,
            45.0,
            Some(1800),
            Some(18000),
        )],
        lanes: vec![QuotaLane {
            name: "gemini-flash".to_string(),
            kind: LaneKind::Own,
            windows: vec![QuotaWindow::new(WindowKind::FiveHour, 15.0, None, None)],
        }],
    }];

    terminal
        .draw(|f| {
            ui::quota_table::render_quota_table(
                &snapshots,
                0,
                Rect::new(0, 0, 100, 20),
                f.buffer_mut(),
            );
        })
        .unwrap();

    let text = buffer_to_text(terminal.backend().buffer());
    assert!(text.contains("Tabela de Quotas (Janelas × Lanes)"));
    assert!(text.contains("Harness"));
    assert!(text.contains("Conta"));
    assert!(text.contains("Uso %"));
    assert!(text.contains("agy"));
    assert!(text.contains("work"));
    assert!(text.contains("45.0%"));
    assert!(text.contains("gemini-flash"));
    assert!(text.contains("15.0%"));
}

#[test]
fn test_render_merge_review() {
    let backend = TestBackend::new(100, 20);
    let mut terminal = Terminal::new(backend).unwrap();

    let diff = "\
diff --git a/src/main.rs b/src/main.rs
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,3 +1,4 @@
-fn old() {}
+fn new() {}
";

    terminal
        .draw(|f| {
            ui::merge::render_merge_review(
                diff,
                "Revise o diff antes de confirmar.",
                MergeStrategy::Squash,
                0,
                Rect::new(0, 0, 100, 20),
                f.buffer_mut(),
            );
        })
        .unwrap();

    let text = buffer_to_text(terminal.backend().buffer());
    assert!(text.contains("Revisão de Merge [Squash]"));
    assert!(text.contains("Revise o diff antes de confirmar."));
    assert!(text.contains("fn old()"));
    assert!(text.contains("fn new()"));
    assert!(text.contains("[s] Squash (selecionado)"));
    assert!(text.contains("[f] Fast-Forward"));
    assert!(text.contains("[k] Keep"));
    assert!(text.contains("[d] Discard"));
    assert!(text.contains("[Enter] Confirmar"));
}

#[test]
fn test_render_terminal_vt100() {
    let backend = TestBackend::new(80, 10);
    let mut terminal = Terminal::new(backend).unwrap();

    let mut vt = vt100::Parser::new(10, 80, 100);
    // Write ANSI colored text: green "PASS", bold "All tests passed"
    vt.process(b"\x1b[32mPASS\x1b[0m \x1b[1mAll tests passed\x1b[0m\r\nSecond line");

    terminal
        .draw(|f| {
            let cursor =
                ui::terminal::render_terminal(vt.screen(), Rect::new(0, 0, 80, 10), f.buffer_mut());
            assert!(cursor.is_some(), "Cursor should be positioned by vt100");
        })
        .unwrap();

    let text = buffer_to_text(terminal.backend().buffer());
    assert!(text.contains("PASS All tests passed"));
    assert!(text.contains("Second line"));

    // Check style of PASS (green) and All (bold)
    let buf = terminal.backend().buffer();
    assert_eq!(buf[(0, 0)].symbol(), "P");
    assert_eq!(buf[(0, 0)].style().fg, Some(Color::Indexed(2))); // ANSI green
    assert_eq!(buf[(5, 0)].symbol(), "A");
    assert!(buf[(5, 0)].style().add_modifier.contains(Modifier::BOLD));
}

#[test]
fn test_render_header_wrap_to_second_line_when_width_constrained() {
    let backend = TestBackend::new(90, 5);
    let mut terminal = Terminal::new(backend).unwrap();

    let mut app = App::new(PathBuf::from("/test/repo"));
    app.snapshots = vec![
        QuotaSnapshot {
            slot: SlotId::new(HarnessId::ClaudeCode, "pro"),
            status: QuotaStatus::Ok,
            source: aihub_core::QuotaSource::Vendor,
            estimated: false,
            note: None,
            windows: vec![QuotaWindow::new(WindowKind::FiveHour, 40.0, None, None)],
            lanes: vec![],
        },
        QuotaSnapshot {
            slot: SlotId::new(HarnessId::Antigravity, "work"),
            status: QuotaStatus::Ok,
            source: aihub_core::QuotaSource::Vendor,
            estimated: false,
            note: None,
            windows: vec![QuotaWindow::new(WindowKind::FiveHour, 50.0, None, None)],
            lanes: vec![],
        },
        QuotaSnapshot {
            slot: SlotId::new(HarnessId::Codex, "corp"),
            status: QuotaStatus::Ok,
            source: aihub_core::QuotaSource::Vendor,
            estimated: false,
            note: None,
            windows: vec![QuotaWindow::new(WindowKind::FiveHour, 60.0, None, None)],
            lanes: vec![],
        },
        QuotaSnapshot {
            slot: SlotId::new(HarnessId::CursorAgent, "team"),
            status: QuotaStatus::Ok,
            source: aihub_core::QuotaSource::Vendor,
            estimated: false,
            note: None,
            windows: vec![QuotaWindow::new(WindowKind::FiveHour, 70.0, None, None)],
            lanes: vec![],
        },
    ];

    // Height 2 allows wrapping to second line
    terminal
        .draw(|f| {
            ui::header::render_header(&app, Rect::new(0, 0, 90, 2), f.buffer_mut());
        })
        .unwrap();

    let text = buffer_to_text(terminal.backend().buffer());

    // Verify all 4 harness slots are rendered without truncation
    assert!(text.contains("claude:"), "Line 1 should contain claude:");
    assert!(text.contains("agy:"), "Should contain agy:");
    assert!(text.contains("codex:"), "Should contain codex:");
    assert!(
        text.contains("cursor-agent:"),
        "Wrapped line should contain cursor-agent:"
    );
    assert!(text.contains('█'), "Should contain gauge blocks");

    // Line 0 and line 1 should both have content
    let lines: Vec<&str> = text.lines().collect();
    assert!(
        lines[0].contains("claude:"),
        "Line 0 has prefix and initial slots"
    );
    assert!(
        lines[1].contains("cursor-agent:") || lines[1].contains("codex:"),
        "Line 1 contains wrapped slots"
    );
}

#[test]
fn test_render_header_no_capacity_informational_banner() {
    let backend = TestBackend::new(120, 5);
    let mut terminal = Terminal::new(backend).unwrap();

    let mut app = App::new(PathBuf::from("/test/repo"));
    app.mode = Mode::Assisted;
    app.recommendation = Some(RecommendationState::NoCapacity {
        reason: "Sem cota disponível em todas as contas".to_string(),
    });

    terminal
        .draw(|f| {
            ui::footer::render_footer(&app, Rect::new(0, 0, 120, 3), f.buffer_mut());
        })
        .unwrap();

    let text = buffer_to_text(terminal.backend().buffer());
    assert!(
        text.contains("SEM CAPACIDADE"),
        "Footer should show SEM CAPACIDADE badge"
    );
    assert!(
        text.contains("Sem cota disponível em todas as contas"),
        "Footer should show reason"
    );
    assert!(
        text.contains("Aceitar indisponível"),
        "Footer should indicate accept is unavailable"
    );
}
