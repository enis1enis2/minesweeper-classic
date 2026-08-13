//! Classic 1992-style Minesweeper frontend built on egui. The board model is
//! the shared `Core.game` (mscore::SimBoard behind the engine state machine);
//! this module is only the view + input layer.

use crate::core::Core;
use crate::engine::{self, DIFF_BEGIN, DIFF_INTERMEDIATE, DIFF_EXPERT};
use crate::telemetry::Telemetry;
use eframe::egui::{
    self, Align2, Color32, FontId, Rect, RichText, Sense, Stroke, Vec2,
};
use std::sync::{Arc, Mutex};

const CELL: f32 = 24.0;
const TOP_H: f32 = 48.0;
const NUM_COLORS: [Color32; 9] = [
    Color32::from_gray(0xC0),
    Color32::from_rgb(0, 0, 0xFF),
    Color32::from_rgb(0, 0x80, 0),
    Color32::from_rgb(0xFF, 0, 0),
    Color32::from_rgb(0, 0, 0x80),
    Color32::from_rgb(0x80, 0, 0),
    Color32::from_rgb(0, 0x80, 0x80),
    Color32::BLACK,
    Color32::from_gray(0x80),
];

pub struct MinesweeperApp {
    core: Arc<Mutex<Core>>,
    telemetry: Option<Telemetry>,
    last_frame: f64,
    show_hof: bool,
    show_seeds: bool,
    show_custom: bool,
    custom_input: [String; 3],
    name_input: String,
}

impl MinesweeperApp {
    pub fn new(core: Arc<Mutex<Core>>, telemetry: Option<Telemetry>) -> Self {
        let name = core.lock().unwrap().player_name.clone();
        MinesweeperApp {
            core,
            telemetry,
            last_frame: 0.0,
            show_hof: false,
            show_seeds: false,
            show_custom: false,
            custom_input: [
                engine::PRESETS[DIFF_INTERMEDIATE].0.to_string(),
                engine::PRESETS[DIFF_INTERMEDIATE].1.to_string(),
                engine::PRESETS[DIFF_INTERMEDIATE].2.to_string(),
            ],
            name_input: name,
        }
    }

    fn update_timer(&mut self, now: f64) {
        let mut c = self.core.lock().unwrap();
        let b = &mut c.game.board;
        if b.started != 0 && b.over == 0 && !c.game.paused {
            let dt = now - self.last_frame;
            if dt > 0.0 {
                c.game.time = (c.game.time as f64 + dt).floor() as usize;
            }
        }
    }

    fn draw_header(&mut self, ui: &mut egui::Ui) {
        let core = self.core.clone();
        let mut c = core.lock().unwrap();
        c.apply_pending_seeds();
        let g = &mut c.game;
        let (mines, flags) = (g.board.mines, g.board.flags);
        let counter = mines.saturating_sub(flags);
        let timer = g.time;
        let over = g.board.over;
        let board_size = Vec2::new(g.board.cols as f32 * CELL, g.board.rows as f32 * CELL);

        let (rect, _) = ui.allocate_exact_size(
            Vec2::new(board_size.x, TOP_H),
            Sense::hover(),
        );
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, Color32::from_gray(0xC0));

        // mine counter (left) and timer (right) panels
        let pad = 8.0;
        let panel_w = 64.0;
        let led = |painter: &egui::Painter, rect: Rect, text: &str, red: bool| {
            painter.rect_filled(rect, 0.0, Color32::BLACK);
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                text,
                FontId::monospace(20.0),
                if red { Color32::from_rgb(0xFF, 0, 0) } else { Color32::from_rgb(0, 0x80, 0) },
            );
        };
        let c_rect = Rect::from_min_size(rect.min + Vec2::new(pad, (TOP_H - 36.0) / 2.0), Vec2::new(panel_w, 36.0));
        led(&painter, c_rect, &format!("{:03}", counter.min(999)), true);
        let t_rect = Rect::from_min_size(
            rect.max - Vec2::new(panel_w + pad, (TOP_H - 36.0) / 2.0) - Vec2::new(0.0, 36.0),
            Vec2::new(panel_w, 36.0),
        );
        led(&painter, t_rect, &format!("{:03}", timer.min(999)), false);

        // smiley
        let face = if over == 1 {
            "8-)".to_string()
        } else if over == -1 {
            "8-D".to_string()
        } else {
            ":-)".to_string()
        };
        let s_rect = Rect::from_center_size(rect.center(), Vec2::new(40.0, 36.0));
        painter.rect_filled(s_rect.shrink(2.0), 0.0, Color32::from_gray(0xD0));
        let s_resp = ui.interact(s_rect.shrink(2.0), ui.id().with("smiley"), Sense::click());
        painter.text(s_rect.center(), Align2::CENTER_CENTER, face, FontId::proportional(20.0), Color32::BLACK);
        if s_resp.clicked() {
            g.reset(g.diff);
        }
    }

    fn draw_board(&mut self, ui: &mut egui::Ui) {
        let core = self.core.clone();
        let (rows, cols) = {
            let c = core.lock().unwrap();
            (c.game.board.rows, c.game.board.cols)
        };
        let size = Vec2::new(cols as f32 * CELL, rows as f32 * CELL);
        let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
        let painter = ui.painter_at(rect);
        let origin = rect.min;

        let mut action: Option<(usize, usize, u8)> = None; // (r, c, op)
        if resp.clicked() {
            if let Some(pos) = resp.interact_pointer_pos() {
                let c = ((pos.x - origin.x) / CELL) as usize;
                let r = ((pos.y - origin.y) / CELL) as usize;
                if r < rows && c < cols {
                    let op = if resp.secondary_clicked() {
                        2
                    } else if resp.middle_clicked() {
                        1
                    } else {
                        0
                    };
                    action = Some((r, c, op));
                }
            }
        }

        let mut c = core.lock().unwrap();
        let g = &mut c.game;
        let b = &mut g.board;
        for r in 0..rows {
            for cc in 0..cols {
                let i = r * cols + cc;
                let cell = Rect::from_min_size(
                    origin + Vec2::new(cc as f32 * CELL, r as f32 * CELL),
                    Vec2::splat(CELL),
                );
                if b.revealed[i] != 0 {
                    painter.rect_filled(cell.shrink(1.0), 0.0, Color32::from_gray(0xB8));
                    painter.rect_stroke(cell.shrink(1.0), 0.0, Stroke::new(1.0_f32, Color32::from_gray(0x88)));
                    if b.mine[i] != 0 {
                        painter.text(cell.center(), Align2::CENTER_CENTER, "*", FontId::proportional(16.0), Color32::BLACK);
                    } else {
                        let n = b.adj[i] as usize;
                        if n > 0 {
                            painter.text(
                                cell.center(),
                                Align2::CENTER_CENTER,
                                n.to_string(),
                                FontId::proportional(16.0),
                                NUM_COLORS[n.min(8)],
                            );
                        }
                    }
                } else {
                    painter.rect_filled(cell.shrink(1.0), 0.0, Color32::from_gray(0xC0));
                    bevel(&painter, cell, true);
                    match b.mark[i] {
                        1 => {
                            painter.text(cell.center(), Align2::CENTER_CENTER, "F", FontId::proportional(16.0), Color32::from_rgb(0xFF, 0, 0));
                        }
                        2 => {
                            painter.text(cell.center(), Align2::CENTER_CENTER, "?", FontId::proportional(16.0), Color32::BLACK);
                        }
                        _ => {}
                    }
                }
            }
        }

        if let Some((r, c, op)) = action {
            if op == 0 {
                let t0 = std::time::Instant::now();
                if g.board.revealed[r * cols + c] != 0 {
                    let _ = g.chord(r as i64, c as i64);
                } else {
                    let _ = g.click(r as i64, c as i64);
                }
                g.note_ui_latency(t0.elapsed().as_micros() as i64);
            } else if op == 1 {
                let _ = g.chord(r as i64, c as i64);
            } else {
                let _ = g.flag(r as i64, c as i64);
            }
        }
    }
}

fn bevel(painter: &egui::Painter, rect: Rect, raised: bool) {
    let a = painter;
    if raised {
        a.line_segment([rect.left_top(), rect.right_top()], Stroke::new(2.0_f32, Color32::from_gray(0xFF)));
        a.line_segment([rect.left_top(), rect.left_bottom()], Stroke::new(2.0_f32, Color32::from_gray(0xFF)));
        a.line_segment([rect.right_top(), rect.right_bottom()], Stroke::new(2.0_f32, Color32::from_gray(0x80)));
        a.line_segment([rect.left_bottom(), rect.right_bottom()], Stroke::new(2.0_f32, Color32::from_gray(0x80)));
    } else {
        a.line_segment([rect.left_top(), rect.right_top()], Stroke::new(1.0_f32, Color32::from_gray(0x80)));
        a.line_segment([rect.left_top(), rect.left_bottom()], Stroke::new(1.0_f32, Color32::from_gray(0x80)));
    }
}

impl eframe::App for MinesweeperApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let now = ctx.input(|i| i.time);
        if self.last_frame == 0.0 {
            self.last_frame = now;
        }
        self.update_timer(now);
        self.last_frame = now;

        if ctx.input(|i| i.key_pressed(egui::Key::P)) {
            self.core.lock().unwrap().game.paused = !self.core.lock().unwrap().game.paused;
        }

        let core = self.core.clone();

        egui::TopBottomPanel::top("menu").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.menu_button("Game", |ui| {
                    if ui.button("New Beginner").clicked() {
                        core.lock().unwrap().game.reset(DIFF_BEGIN);
                        ui.close_menu();
                    }
                    if ui.button("New Intermediate").clicked() {
                        core.lock().unwrap().game.reset(DIFF_INTERMEDIATE);
                        ui.close_menu();
                    }
                    if ui.button("New Expert").clicked() {
                        core.lock().unwrap().game.reset(DIFF_EXPERT);
                        ui.close_menu();
                    }
                    if ui.button("Custom...").clicked() {
                        self.show_custom = true;
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("Restart").clicked() {
                        let d = core.lock().unwrap().game.diff;
                        core.lock().unwrap().game.reset(d);
                        ui.close_menu();
                    }
                    if ui.button("Quit").clicked() {
                        ui.close_menu();
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
                ui.menu_button("Options", |ui| {
                    let mut marks = core.lock().unwrap().game.marks_enabled;
                    if ui.checkbox(&mut marks, "Question marks").changed() {
                        core.lock().unwrap().game.set_marks(marks);
                    }
                    let mut paused = core.lock().unwrap().game.paused;
                    if ui.checkbox(&mut paused, "Pause").changed() {
                        core.lock().unwrap().game.paused = paused;
                    }
                });
                ui.menu_button("Solver", |ui| {
                    if ui.button("Leaderboard...").clicked() {
                        self.show_hof = true;
                        if let Some(t) = &self.telemetry {
                            t.request_lbtop(None, 10);
                        }
                        ui.close_menu();
                    }
                    if ui.button("Seeds...").clicked() {
                        self.show_seeds = true;
                        ui.close_menu();
                    }
                });
                ui.separator();
                let st = core.lock().unwrap().lb_status.clone();
                ui.label(RichText::new(st).small());
            });
        });

        if self.show_custom {
            egui::Window::new("Custom Board")
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Rows");
                        ui.text_edit_singleline(&mut self.custom_input[0]);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Cols");
                        ui.text_edit_singleline(&mut self.custom_input[1]);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Mines");
                        ui.text_edit_singleline(&mut self.custom_input[2]);
                    });
                    ui.horizontal(|ui| {
                        if ui.button("OK").clicked() {
                            let r = self.custom_input[0].parse::<i64>().unwrap_or(8);
                            let c = self.custom_input[1].parse::<i64>().unwrap_or(8);
                            let m = self.custom_input[2].parse::<i64>().unwrap_or(10);
                            core.lock().unwrap().game.reset_custom(r, c, m);
                            self.show_custom = false;
                        }
                        if ui.button("Cancel").clicked() {
                            self.show_custom = false;
                        }
                    });
                });
        }

        if self.show_seeds {
            let state = {
                let c = core.lock().unwrap();
                c.game.cmd_seeds()
            };
            egui::Window::new("Seeds")
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.monospace(state);
                    ui.horizontal(|ui| {
                        ui.label("Name");
                        ui.text_edit_singleline(&mut self.name_input);
                    });
                    let mut auto = core.lock().unwrap().auto_submit;
                    if ui.checkbox(&mut auto, "Submit wins").changed() {
                        core.lock().unwrap().auto_submit = auto;
                    }
                });
            let mut c = core.lock().unwrap();
            if c.player_name != self.name_input {
                c.player_name = self.name_input.clone();
            }
        }

        if self.show_hof {
            let (entries, status) = {
                let c = core.lock().unwrap();
                (c.leaderboard.clone(), c.lb_status.clone())
            };
            egui::Window::new("Hall of Fame")
                .collapsible(false)
                .resizable(true)
                .default_size(Vec2::new(340.0, 260.0))
                .show(ctx, |ui| {
                    ui.label(RichText::new(status).strong());
                    ui.separator();
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        for e in &entries {
                            ui.horizontal(|ui| {
                                ui.monospace(format!("{:>3}.", e.rank));
                                ui.monospace(format!("{:<5}", e.diff));
                                ui.monospace(&e.name);
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        ui.monospace(format!("{:4.1}", e.time_ms as f64 / 1000.0));
                                    },
                                );
                            });
                        }
                    });
                    ui.separator();
                    ui.horizontal(|ui| {
                        if ui.button("Beginner").clicked() {
                            if let Some(t) = &self.telemetry {
                                t.request_lbtop(Some(DIFF_BEGIN), 10);
                            }
                        }
                        if ui.button("Intermediate").clicked() {
                            if let Some(t) = &self.telemetry {
                                t.request_lbtop(Some(DIFF_INTERMEDIATE), 10);
                            }
                        }
                        if ui.button("Expert").clicked() {
                            if let Some(t) = &self.telemetry {
                                t.request_lbtop(Some(DIFF_EXPERT), 10);
                            }
                        }
                    });
                });
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                self.draw_header(ui);
                self.draw_board(ui);
                let st = {
                    let c = core.lock().unwrap();
                    if c.game.board.over == 1 {
                        format!("You win! {} seconds.", c.game.time)
                    } else if c.game.board.over == -1 {
                        "Game over!".to_string()
                    } else {
                        format!("{} {}", engine::DIFF_NAMES[c.game.diff], c.game.time)
                    }
                };
                ui.label(RichText::new(st).strong());
            });
        });

        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }
}
