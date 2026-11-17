//! Dockable AI Video Editing Copilot panel powered by `nle-ai`.
//! Supports Google Gemini 4 (Hydrogen / Argon / Carbon + thinkingLevel + 480p proxy),
//! OpenAI / OpenRouter, Local Ollama, and offline deterministic tool execution.

use crate::app::App;
use crate::theme;
use eframe::egui::{self, Color32, CornerRadius, Stroke};
use nle_ai::connection::{ConnectionReport, ModelInfo};
use nle_ai::{AiConfig, AiProviderKind, ChatMessage, CopilotError, CopilotResponse, ThinkingLevel, DEFAULT_GEMINI_MODEL, QUICK_PROMPTS};
use nle_core::ClipKind;
use nle_media::jobs::Job;
use std::time::Duration;

pub struct CopilotState {
    pub open: bool,
    pub config: AiConfig,
    pub prompt: String,
    pub messages: Vec<ChatMessage>,
    pub job: Option<Job<Result<CopilotResponse, CopilotError>>>,
    pub show_settings: bool,
    pub last_notice: Option<String>,
    /// Prompt of the last failed request, offered via the Retry button.
    pub retry_prompt: Option<String>,
    /// Prompt of the in-flight request.
    pub pending_prompt: Option<String>,
    /// "Test Live Connection" in flight / last result.
    pub test_job: Option<Job<ConnectionReport>>,
    pub test_report: Option<ConnectionReport>,
    /// "Refresh model list" in flight / last result.
    pub models_job: Option<Job<Vec<ModelInfo>>>,
    pub live_models: Vec<ModelInfo>,
    pub models_error: Option<String>,
    pub save_error: Option<String>,
}

impl Default for CopilotState {
    fn default() -> Self {
        Self {
            open: false,
            config: AiConfig::load(),
            prompt: String::new(),
            messages: Vec::new(),
            job: None,
            show_settings: false,
            last_notice: None,
            retry_prompt: None,
            pending_prompt: None,
            test_job: None,
            test_report: None,
            models_job: None,
            live_models: Vec::new(),
            models_error: None,
            save_error: None,
        }
    }
}

pub fn show(app: &mut App, ctx: &egui::Context) {
    if !app.copilot.open {
        return;
    }
    let mut open = app.copilot.open;
    let mut send_prompt: Option<String> = None;
    let mut save_cfg = false;
    let mut do_undo = false;
    let mut do_cancel = false;
    let mut do_test = false;
    let mut do_list_models = false;

    egui::SidePanel::right("ai_copilot_panel")
        .resizable(true)
        .default_width(340.0)
        .min_width(290.0)
        .frame(egui::Frame::new().fill(theme::PANEL).inner_margin(egui::Margin::same(8)).stroke(Stroke::new(1.0_f32, theme::BORDER)))
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("🤖 AI Copilot").strong().size(14.0).color(theme::VIDEO));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("✕").on_hover_text("Close Copilot (Ctrl+Shift+A)").clicked() {
                        open = false;
                    }
                    if ui.add_enabled(app.history.can_undo(), egui::Button::new("↶ Undo").small()).clicked() {
                        do_undo = true;
                    }
                    ui.toggle_value(&mut app.copilot.show_settings, "⚙ Config");
                });
            });
            ui.separator();

            if app.copilot.show_settings {
                egui::Frame::new()
                    .fill(Color32::from_rgb(0x16, 0x17, 0x1B))
                    .corner_radius(CornerRadius::same(4))
                    .inner_margin(egui::Margin::same(8))
                    .show(ui, |ui| {
                        let live_models = &app.copilot.live_models;
                        let listing = app.copilot.models_job.is_some();
                        let cfg = &mut app.copilot.config;
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("Provider").small().color(theme::TEXT_DIM));
                            egui::ComboBox::from_id_salt("ai-provider")
                                .selected_text(cfg.provider.label())
                                .show_ui(ui, |ui| {
                                    for p in AiProviderKind::ALL {
                                        ui.selectable_value(&mut cfg.provider, p, p.label());
                                    }
                                });
                        });
                        match cfg.provider {
                            AiProviderKind::Gemini4 => {
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new("Model").small().color(theme::TEXT_DIM));
                                    egui::ComboBox::from_id_salt("gemini-model")
                                        .selected_text(&cfg.gemini_model)
                                        .width(170.0)
                                        .show_ui(ui, |ui| {
                                            // the saved choice and the default stay pickable even before the live list loads
                                            let mut pinned = vec![cfg.gemini_model.clone()];
                                            if cfg.gemini_model != DEFAULT_GEMINI_MODEL {
                                                pinned.push(DEFAULT_GEMINI_MODEL.to_string());
                                            }
                                            for id in pinned.iter().filter(|id| !live_models.iter().any(|m| &m.id == *id)) {
                                                let hint = if id == DEFAULT_GEMINI_MODEL { "default" } else { "saved choice" };
                                                ui.selectable_value(&mut cfg.gemini_model, id.clone(), format!("{id} ({hint})"));
                                            }
                                            if live_models.is_empty() {
                                                ui.label(egui::RichText::new("Press ⟳ to list the models your key can use").small().color(theme::TEXT_DIM));
                                            } else {
                                                ui.separator();
                                                ui.label(egui::RichText::new("Available to your key").small().color(theme::TEXT_DIM));
                                                for m in live_models.iter() {
                                                    ui.selectable_value(&mut cfg.gemini_model, m.id.clone(), &m.id).on_hover_text(&m.display_name);
                                                }
                                            }
                                        });
                                    if ui
                                        .add_enabled(!listing, egui::Button::new(if listing { "…" } else { "⟳" }).small())
                                        .on_hover_text("Refresh model list from Google (needs the API key)")
                                        .clicked()
                                    {
                                        do_list_models = true;
                                    }
                                });
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new("Custom").small().color(theme::TEXT_DIM));
                                    ui.add(egui::TextEdit::singleline(&mut cfg.gemini_custom_model).hint_text("optional model id override").desired_width(170.0))
                                        .on_hover_text("If set, this model id is used instead of the dropdown (e.g. a newer preview model)");
                                });
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new("Fallback").small().color(theme::TEXT_DIM));
                                    ui.add(egui::TextEdit::singleline(&mut cfg.gemini_fallback_model).hint_text("empty = no fallback").desired_width(170.0))
                                        .on_hover_text("Used once when the main model answers 'busy' (HTTP 503/429)");
                                });
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new("Thinking").small().color(theme::TEXT_DIM));
                                    egui::ComboBox::from_id_salt("gemini-thinking")
                                        .selected_text(cfg.thinking_level.label())
                                        .show_ui(ui, |ui| {
                                            for lvl in ThinkingLevel::ALL {
                                                ui.selectable_value(&mut cfg.thinking_level, lvl, lvl.label());
                                            }
                                        });
                                });
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new("API Key").small().color(theme::TEXT_DIM));
                                    ui.add(egui::TextEdit::singleline(&mut cfg.gemini_api_key).password(true).hint_text("GEMINI_API_KEY or paste here"));
                                });
                                ui.checkbox(&mut cfg.attach_video_proxy, "Upload 480p 15fps proxy video for visual context");
                            }
                            AiProviderKind::OpenAiCompatible => {
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new("Base URL").small().color(theme::TEXT_DIM));
                                    ui.text_edit_singleline(&mut cfg.openai_base_url);
                                });
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new("Model").small().color(theme::TEXT_DIM));
                                    ui.text_edit_singleline(&mut cfg.openai_model);
                                });
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new("API Key").small().color(theme::TEXT_DIM));
                                    ui.add(egui::TextEdit::singleline(&mut cfg.openai_api_key).password(true));
                                });
                            }
                            AiProviderKind::Ollama => {
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new("Endpoint").small().color(theme::TEXT_DIM));
                                    ui.text_edit_singleline(&mut cfg.ollama_url);
                                });
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new("Model").small().color(theme::TEXT_DIM));
                                    ui.text_edit_singleline(&mut cfg.ollama_model);
                                });
                            }
                        }
                        if nle_ai::config::secrets::SECURE {
                            ui.label(egui::RichText::new("🔒 API keys are stored in Windows Credential Manager, not in the config file.").small().color(theme::TEXT_DIM));
                        }
                        ui.horizontal(|ui| {
                            if ui.button("💾 Save AI Config").clicked() {
                                save_cfg = true;
                            }
                            let testing = app.copilot.test_job.is_some();
                            if ui
                                .add_enabled(!testing, egui::Button::new(if testing { "Testing…" } else { "📡 Test Live Connection" }))
                                .on_hover_text("Checks the key, that the model exists, and sends a tiny request (uses a few tokens)")
                                .clicked()
                            {
                                do_test = true;
                            }
                        });
                        if let Some(e) = &app.copilot.save_error {
                            ui.colored_label(Color32::from_rgb(0xE5, 0x6B, 0x6B), egui::RichText::new(format!("Save failed: {e}")).small());
                        }
                        if let Some(e) = &app.copilot.models_error {
                            ui.colored_label(Color32::from_rgb(0xE5, 0x6B, 0x6B), egui::RichText::new(format!("Model list: {e}")).small());
                        }
                        if let Some(report) = &app.copilot.test_report {
                            for line in &report.lines {
                                let (icon, color) = if line.ok { ("✔", Color32::from_rgb(0x6B, 0xD3, 0x8A)) } else { ("✖", Color32::from_rgb(0xE5, 0x6B, 0x6B)) };
                                ui.colored_label(color, egui::RichText::new(format!("{icon} {}", line.text)).small());
                            }
                        }
                    });
                ui.add_space(4.0);
            }

            ui.label(egui::RichText::new("Quick Montage Actions").small().color(theme::TEXT_DIM));
            ui.horizontal_wrapped(|ui| {
                let busy = app.copilot.job.is_some();
                for (chip_label, chip_prompt) in QUICK_PROMPTS {
                    if ui.add_enabled(!busy, egui::Button::new(egui::RichText::new(chip_label).small())).clicked() {
                        send_prompt = Some(chip_prompt.to_string());
                    }
                }
            });
            ui.separator();

            let bottom_h = 74.0;
            let chat_h = (ui.available_height() - bottom_h).max(120.0);
            egui::ScrollArea::vertical()
                .max_height(chat_h)
                .stick_to_bottom(true)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if app.copilot.messages.is_empty() {
                        ui.label(
                            egui::RichText::new(
                                "Ask Argon Copilot to locate kills, sync edits to music beats, apply CS2/Valorant presets, or switch to 9:16 Split-Crop.\nEvery timeline action supports Ctrl+Z undo.",
                            )
                            .small()
                            .color(theme::TEXT_DIM),
                        );
                    }
                    for msg in &app.copilot.messages {
                        let is_user = msg.role == "user";
                        let fill = if is_user {
                            Color32::from_rgb(0x20, 0x33, 0x4A)
                        } else {
                            Color32::from_rgb(0x1B, 0x1D, 0x22)
                        };
                        egui::Frame::new()
                            .fill(fill)
                            .corner_radius(CornerRadius::same(6))
                            .inner_margin(egui::Margin::same(8))
                            .show(ui, |ui| {
                                ui.label(egui::RichText::new(if is_user { "You" } else { "Argon Copilot" }).small().strong().color(if is_user { theme::VIDEO } else { theme::SNAP }));
                                ui.label(&msg.text);
                                if !msg.outcomes.is_empty() {
                                    ui.add_space(3.0);
                                    for o in &msg.outcomes {
                                        let (fill, color, badge) = if o.applied {
                                            (Color32::from_rgb(0x13, 0x2A, 0x1C), Color32::from_rgb(0x4A, 0xDE, 0x80), "✓ Applied".to_string())
                                        } else {
                                            (Color32::from_rgb(0x33, 0x27, 0x10), Color32::from_rgb(0xF5, 0xB0, 0x2E), "Skipped".to_string())
                                        };
                                        egui::Frame::new()
                                            .fill(fill)
                                            .corner_radius(CornerRadius::same(3))
                                            .inner_margin(egui::Margin::symmetric(5, 2))
                                            .show(ui, |ui| {
                                                ui.horizontal_wrapped(|ui| {
                                                    ui.label(egui::RichText::new(badge).small().strong().color(color));
                                                    ui.label(egui::RichText::new(&o.tool).monospace().small().color(theme::TEXT_DIM));
                                                    ui.label(egui::RichText::new(&o.detail).small().color(if o.applied { theme::TEXT_DIM } else { color }));
                                                });
                                            });
                                        ui.add_space(2.0);
                                    }
                                } else if !msg.tool_calls.is_empty() {
                                    ui.add_space(3.0);
                                    ui.horizontal_wrapped(|ui| {
                                        for tc in &msg.tool_calls {
                                            egui::Frame::new()
                                                .fill(Color32::from_rgb(0x13, 0x24, 0x1D))
                                                .corner_radius(CornerRadius::same(3))
                                                .inner_margin(egui::Margin::symmetric(5, 2))
                                                .show(ui, |ui| {
                                                    ui.label(egui::RichText::new(format!("⚡ {}", tc.name())).monospace().small().color(theme::SNAP));
                                                });
                                        }
                                    });
                                }
                            });
                        ui.add_space(4.0);
                    }
                    if let Some(job) = &app.copilot.job {
                        let (_, msg) = job.progress();
                        ui.horizontal(|ui| {
                            ui.spinner();
                            let msg = if msg.is_empty() { "Thinking & preparing timeline edits…".to_string() } else { msg };
                            ui.label(egui::RichText::new(msg).small().color(theme::TEXT_DIM));
                            if ui.small_button("Cancel").clicked() {
                                do_cancel = true;
                            }
                        });
                    } else if app.copilot.retry_prompt.is_some() && ui.button("⟳ Retry").on_hover_text("Send the last request again").clicked() {
                        send_prompt = app.copilot.retry_prompt.clone();
                    }
                });

            ui.separator();
            ui.horizontal(|ui| {
                let busy = app.copilot.job.is_some();
                let te = ui.add_enabled(
                    !busy,
                    egui::TextEdit::multiline(&mut app.copilot.prompt)
                        .desired_rows(2)
                        .desired_width(ui.available_width() - 62.0)
                        .hint_text("Describe montage edit…"),
                );
                let enter_send = te.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift);
                let clicked = ui.add_enabled(!busy && !app.copilot.prompt.trim().is_empty(), egui::Button::new("Send").min_size(egui::vec2(52.0, 40.0))).clicked();
                if (clicked || enter_send) && !app.copilot.prompt.trim().is_empty() {
                    send_prompt = Some(std::mem::take(&mut app.copilot.prompt));
                }
            });
            if let Some(note) = &app.copilot.last_notice {
                ui.label(egui::RichText::new(note).small().color(theme::TEXT_DIM));
            }
        });

    app.copilot.open = open;
    if save_cfg {
        match app.copilot.config.save() {
            Ok(()) => {
                app.copilot.save_error = None;
                app.copilot.last_notice = Some("Saved AI Copilot config".into());
                app.status("Saved AI Copilot configuration");
            }
            Err(e) => {
                app.status(format!("Save config failed: {e}"));
                app.copilot.save_error = Some(e.to_string());
            }
        }
    }
    if do_test {
        let cfg = app.copilot.config.clone();
        app.copilot.test_report = None;
        app.copilot.test_job = Some(Job::spawn("ai-connection-test", move |_| Ok(nle_ai::connection::test_connection(&cfg))));
    }
    if do_list_models {
        match app.copilot.config.effective_gemini_key() {
            Some(key) => {
                app.copilot.models_error = None;
                app.copilot.models_job = Some(Job::spawn("gemini-models", move |_| nle_ai::connection::list_gemini_models(&key)));
            }
            None => app.copilot.models_error = Some("set an API key first".into()),
        }
    }
    if do_undo {
        app.undo();
    }
    if do_cancel {
        if let Some(job) = &app.copilot.job {
            job.cancel();
        }
    }
    if let Some(p) = send_prompt {
        submit_prompt(app, p);
    }
}

pub fn submit_prompt(app: &mut App, prompt: String) {
    let trimmed = prompt.trim().to_string();
    if trimmed.is_empty() || app.copilot.job.is_some() {
        return;
    }
    // a retry re-sends the same prompt; don't duplicate the user bubble
    let is_retry = app.copilot.retry_prompt.take().as_deref() == Some(trimmed.as_str());
    if !is_retry {
        app.copilot.messages.push(ChatMessage {
            role: "user".into(),
            text: trimmed.clone(),
            tool_calls: Vec::new(),
            outcomes: Vec::new(),
        });
    }

    let active_video_path = app.target_clip().and_then(|cid| {
        let clip = app.project.timeline.clip(cid)?;
        if let ClipKind::Asset { asset } = clip.kind {
            let a = app.project.asset(asset)?;
            if a.has_video() && a.path.is_file() {
                return Some(a.path.clone());
            }
        }
        None
    });

    let cfg = app.copilot.config.clone();
    let tl = app.project.timeline.clone();
    let assets = app.project.assets.clone();
    let playhead = app.playhead;
    let selection = app.selection.clone();

    let retry_prompt = trimmed.clone();
    app.copilot.job = Some(Job::spawn("ai-copilot", move |ctx| {
        let cancelled = ctx.cancel_flag();
        let status = ctx.message_fn();
        Ok(nle_ai::run_copilot_blocking(cfg, trimmed, tl, assets, playhead, selection, active_video_path, &cancelled, &status))
    }));
    app.copilot.last_notice = None;
    // remembered until the request succeeds, so a failure can be retried
    app.copilot.retry_prompt = None;
    app.copilot.pending_prompt = Some(retry_prompt);
    app.status("Sending request to AI Copilot…");
}

fn push_error(app: &mut App, message: String, retryable: bool) {
    let prompt = app.copilot.pending_prompt.take();
    let hint = if retryable { " — you can retry." } else { "" };
    app.copilot.messages.push(ChatMessage {
        role: "assistant".into(),
        text: format!("⚠ Copilot request failed: {message}{hint}\nNo timeline changes were made."),
        tool_calls: Vec::new(),
        outcomes: Vec::new(),
    });
    // non-retryable errors (bad key, 4xx) can still be re-sent after fixing the config
    app.copilot.retry_prompt = if message == "Cancelled" { None } else { prompt };
    app.status(format!("AI Copilot failed: {message}"));
}

/// Finish the settings-panel background checks.
fn poll_settings_jobs(app: &mut App, ctx: &egui::Context) {
    if let Some(job) = &app.copilot.test_job {
        match job.take() {
            Some(res) => {
                app.copilot.test_job = None;
                let report = res.unwrap_or_else(|e| ConnectionReport {
                    lines: vec![nle_ai::connection::CheckLine { ok: false, text: e }],
                });
                app.status(if report.ok() { "AI connection OK".to_string() } else { "AI connection test found a problem — see Copilot settings".to_string() });
                app.copilot.test_report = Some(report);
            }
            None => ctx.request_repaint_after(Duration::from_millis(100)),
        }
    }
    if let Some(job) = &app.copilot.models_job {
        match job.take() {
            Some(Ok(models)) => {
                app.copilot.models_job = None;
                app.status(format!("{} Gemini models available to this key", models.len()));
                app.copilot.live_models = models;
            }
            Some(Err(e)) => {
                app.copilot.models_job = None;
                app.copilot.models_error = Some(e);
            }
            None => ctx.request_repaint_after(Duration::from_millis(100)),
        }
    }
}

pub fn poll(app: &mut App, ctx: &egui::Context) {
    poll_settings_jobs(app, ctx);
    let Some(job) = app.copilot.job.as_ref() else { return };
    if let Some(res) = job.take() {
        app.copilot.job = None;
        match res {
            Ok(Ok(resp)) => {
                app.copilot.pending_prompt = None;
                // Validate + apply against the *live* timeline as one atomic undo step.
                let mut outcomes = resp.rejected.clone();
                outcomes.extend(nle_ai::apply_copilot_actions(
                    &resp.tool_calls,
                    &mut app.project.timeline,
                    &mut app.history,
                    &mut app.playhead,
                    &app.selection,
                ));
                let applied = outcomes.iter().filter(|o| o.applied).count();
                let skipped = outcomes.len() - applied;
                let mut body = resp.reply;
                if body.trim().is_empty() {
                    body = if outcomes.is_empty() { "No timeline changes requested.".into() } else { String::new() };
                }
                app.copilot.last_notice = Some(format!("Provider: {}", resp.provider_label));
                app.copilot.messages.push(ChatMessage {
                    role: "assistant".into(),
                    text: body,
                    tool_calls: resp.tool_calls,
                    outcomes,
                });
                app.status(match (applied, skipped) {
                    (0, 0) => format!("AI Copilot replied ({})", resp.provider_label),
                    (a, 0) => format!("AI Copilot applied {a} action(s) — Ctrl+Z undoes all"),
                    (a, s) => format!("AI Copilot applied {a}, skipped {s} — Ctrl+Z undoes all"),
                });
            }
            Ok(Err(e)) => push_error(app, e.message, e.retryable),
            Err(e) => push_error(app, e, true),
        }
    } else {
        ctx.request_repaint_after(Duration::from_millis(50));
    }
}
