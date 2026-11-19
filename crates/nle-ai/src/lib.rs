//! Argon Cloud & Local AI Video Editing Copilot (`nle-ai`).
//!
//! Supports Google Gemini (default `gemini-4-argon`; the model picker is filled
//! from the key's live model list) with configurable `thinkingLevel` (`LOW` / `MEDIUM` / `HIGH`) and 480p/15fps
//! proxy upload via Gemini File API, OpenAI/OpenRouter Chat Completions, Local
//! Ollama, structured undoable timeline tools, and an offline deterministic fallback.

pub mod config;
pub mod connection;
pub mod multimodal;
pub mod providers;
pub mod segment;
pub mod tools;
pub mod tools_post;
#[cfg(feature = "neural-voice")]
pub mod voice;

pub use config::{AiConfig, AiProviderKind, ThinkingLevel, DEFAULT_GEMINI_MODEL};
pub use multimodal::{build_timeline_json, encode_proxy_480p15, upload_gemini_file};
pub use providers::{
    build_gemini_request_body, parse_gemini_response, parse_ollama_response, parse_openai_response, run_copilot_blocking, ChatMessage, CopilotError, CopilotResponse,
};
pub use tools::{apply_copilot_actions, apply_copilot_actions_with, execute_tool_call, offline_copilot_plan, ActionOutcome, CopilotEnv, CopilotToolCall, QUICK_PROMPTS};
pub use tools_post::UiRequest;

#[cfg(test)]
mod tests {
    use super::*;
    use nle_core::{Clip, History, MediaAsset, Time, Timeline};
    use serde_json::json;

    #[test]
    fn ai_config_toml_round_trip() {
        let dir = std::env::temp_dir().join(format!("argon_ai_cfg_{}", std::process::id()));
        let path = dir.join("ai_config.toml");
        let cfg = AiConfig {
            provider: AiProviderKind::Gemini4,
            gemini_api_key: "test-key-123".into(),
            gemini_model: "gemini-flash-latest".into(),
            thinking_level: ThinkingLevel::High,
            attach_video_proxy: true,
            ..Default::default()
        };
        cfg.save_to_path(&path).unwrap();
        let loaded = AiConfig::load_from_path(&path);
        assert_eq!(loaded, cfg);
        assert_eq!(loaded.thinking_level.as_gemini_str(), "HIGH");
        assert_eq!(loaded.gemini_model, "gemini-flash-latest");
        assert_eq!(AiConfig::default().gemini_model, DEFAULT_GEMINI_MODEL);
        assert_eq!(DEFAULT_GEMINI_MODEL, "gemini-4-argon");
        // the key went to the (test-only) credential store, not the file
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.contains("test-key-123"), !crate::config::secrets::SECURE);
        let mut cleared = cfg.clone();
        cleared.gemini_api_key.clear();
        cleared.save_to_path(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gemini_request_and_response_tool_calls() {
        let mut tl = Timeline::default();
        let asset = MediaAsset::new("valorant_ace.mp4", "/clips/val.mp4", Time::from_secs(10));
        tl.append(Clip::from_asset(&asset)).unwrap();
        tl.markers.push(nle_core::Marker::kill(Time::from_secs(4), "Kill 1"));

        let snapshot = build_timeline_json(&tl, &[asset], Time::from_secs(2), &[]);
        assert_eq!(snapshot["clips"].as_array().unwrap().len(), 1);
        assert_eq!(snapshot["markers"][0]["kind"], "Kill");

        let cfg = AiConfig { gemini_model: DEFAULT_GEMINI_MODEL.into(), thinking_level: ThinkingLevel::High, ..Default::default() };
        let req = build_gemini_request_body(&cfg, "Apply flow sync", &snapshot, Some("https://example.invalid/files/123"));
        assert_eq!(req["generationConfig"]["thinkingConfig"]["thinkingLevel"], "HIGH");
        assert_eq!(req["contents"][0]["parts"][0]["fileData"]["fileUri"], "https://example.invalid/files/123");

        let fake_resp = json!({
            "candidates": [{
                "content": {
                    "parts": [
                        { "thought": true, "text": "internal thought" },
                        { "text": "Applying velocity curve and shake." },
                        { "functionCall": { "name": "apply_velocity_preset", "args": { "preset_name": "Valorant Flow Sync" } } },
                        { "functionCall": { "name": "drop_marker", "args": { "time_secs": 3.5, "label": "Headshot", "kind": "kill" } } }
                    ]
                }
            }]
        });
        let (reply, calls, rejected) = parse_gemini_response(&fake_resp).unwrap();
        assert_eq!(reply, "Applying velocity curve and shake.");
        assert_eq!(calls.len(), 2);
        assert!(rejected.is_empty());
        assert_eq!(calls[0].name(), "apply_velocity_preset");
        assert_eq!(calls[1].name(), "drop_marker");

        // grounding: bounds + allowed vocab are part of the snapshot
        assert_eq!(snapshot["bounds_secs"][1], 10.0);
        assert!(snapshot["allowed"]["transition_kinds"].as_array().unwrap().iter().any(|v| v == "whip_pan"));
        assert!(snapshot["allowed"]["effect_presets"].as_array().unwrap().iter().any(|v| v == "Gaming 9:16 Split-Crop"));
    }

    #[test]
    fn invalid_model_calls_are_rejected_with_reasons_not_dropped() {
        let resp = json!({
            "candidates": [{ "content": { "parts": [
                { "functionCall": { "name": "drop_marker", "args": { "time_secs": "abc" } } },
                { "functionCall": { "name": "delete_everything", "args": {} } },
                { "functionCall": { "name": "add_transition", "args": { "transition_kind": "wormhole" } } },
                { "functionCall": { "name": "jump_to_timecode", "args": { "time_secs": "2.5s" } } }
            ] } }]
        });
        let (_, calls, rejected) = parse_gemini_response(&resp).unwrap();
        assert_eq!(calls, vec![CopilotToolCall::JumpToTimecode { time_secs: 2.5 }]);
        assert_eq!(rejected.len(), 3);
        assert!(rejected.iter().all(|o| !o.applied && !o.detail.is_empty()));

        let oa = json!({ "choices": [{ "message": { "content": "", "tool_calls": [
            { "function": { "name": "drop_marker", "arguments": "{not json" } },
            { "function": { "name": "jump_to_timecode", "arguments": "{\"time_secs\": 1}" } }
        ] } }] });
        let (_, calls, rejected) = parse_openai_response(&oa).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(rejected.len(), 1);

        assert!(parse_gemini_response(&json!({ "promptFeedback": { "blockReason": "SAFETY" } })).is_err());
        assert!(parse_gemini_response(&json!({ "candidates": [{ "finishReason": "MAX_TOKENS" }] })).is_err());
    }

    #[test]
    fn copilot_run_is_validated_atomic_and_one_undo_step() {
        let mut tl = Timeline::default();
        let asset = MediaAsset::new("val.mp4", "/val.mp4", Time::from_secs(8));
        let id = tl.append(Clip::from_asset(&asset)).unwrap();
        let original = tl.clone();
        let mut history = History::new(64);
        let mut playhead = Time::from_secs(1);

        let calls = vec![
            CopilotToolCall::DropMarker { time_secs: 2.0, label: "Kill".into(), kind: "kill".into() },
            CopilotToolCall::CutAndRipple { clip_id: Some(id.0), time_secs: 99.0, trim_mode: "blade".into() },
            CopilotToolCall::ApplyVelocityPreset { clip_id: Some(424242), preset_name: "Valorant Flow Sync".into() },
            CopilotToolCall::ApplyVelocityPreset { clip_id: Some(id.0), preset_name: "Not A Preset".into() },
            CopilotToolCall::JumpToTimecode { time_secs: -3.0 },
            CopilotToolCall::CutAndRipple { clip_id: Some(id.0), time_secs: 4.0, trim_mode: "blade".into() },
        ];
        let out = apply_copilot_actions(&calls, &mut tl, &mut history, &mut playhead, &[id]);
        let applied: Vec<bool> = out.iter().map(|o| o.applied).collect();
        assert_eq!(applied, vec![true, false, false, false, false, true], "{out:#?}");
        assert!(out.iter().filter(|o| !o.applied).all(|o| !o.detail.is_empty()));
        assert_eq!(tl.spine.len(), 2);
        assert_eq!(tl.markers.len(), 1);
        assert!(playhead <= tl.extent());

        // one Ctrl+Z reverts the whole Copilot run
        assert!(history.undo(&mut tl));
        assert!(!history.can_undo());
        assert_eq!(tl.spine.len(), original.spine.len());
        assert!(tl.markers.is_empty());
    }

    #[test]
    fn timeline_context_caps_markers_near_playhead() {
        let mut tl = Timeline::default();
        let asset = MediaAsset::new("long.mp4", "/long.mp4", Time::from_secs(600));
        tl.append(Clip::from_asset(&asset)).unwrap();
        for i in 0..1000 {
            tl.markers.push(nle_core::Marker::beat(Time::from_millis(i * 500)));
        }
        let snap = build_timeline_json(&tl, &[asset], Time::from_secs(400), &[]);
        let m = snap["markers"].as_array().unwrap();
        assert_eq!(m.len(), 300);
        assert_eq!(snap["markers_total"], 1000);
        assert!(m.iter().all(|v| (v["time_secs"].as_f64().unwrap() - 400.0).abs() <= 80.0));
    }

    #[test]
    fn unreachable_provider_returns_retryable_error_without_editing() {
        let cfg = AiConfig { provider: AiProviderKind::Ollama, ollama_url: "http://127.0.0.1:9".into(), ..Default::default() };
        let mut tl = Timeline::default();
        let asset = MediaAsset::new("c.mp4", "/c.mp4", Time::from_secs(5));
        tl.append(Clip::from_asset(&asset)).unwrap();
        let never = || false;
        let quiet = |_: String| {};
        let err = run_copilot_blocking(cfg.clone(), "cut it".into(), tl.clone(), vec![], Time::ZERO, vec![], None, &never, &quiet).unwrap_err();
        assert!(err.retryable, "{err}");

        let cancelled = || true;
        let err = run_copilot_blocking(cfg, "cut it".into(), tl, vec![], Time::ZERO, vec![], None, &cancelled, &quiet).unwrap_err();
        assert_eq!(err.message, "Cancelled");
    }

    #[test]
    fn all_seven_tools_execute_and_undo_cleanly() {
        let mut tl = Timeline::default();
        let asset = MediaAsset::new("cs2.mp4", "/clips/cs2.mp4", Time::from_secs(10));
        let mut c = Clip::from_asset(&asset);
        c.duration = Time::from_secs(8);
        let id = tl.append(c).unwrap();
        tl.markers.push(nle_core::Marker::beat(Time::from_secs(3)));

        let mut history = History::new(64);
        let mut playhead = Time::from_secs(2);
        let selection = vec![id];

        let calls = vec![
            CopilotToolCall::JumpToTimecode { time_secs: 3.0 },
            CopilotToolCall::ApplyVelocityPreset { clip_id: Some(id.0), preset_name: "Valorant Flow Sync".into() },
            CopilotToolCall::AddEffectPreset { clip_id: Some(id.0), preset_name: "Hard Kill Impact (S_Shake Decay)".into() },
            CopilotToolCall::DropMarker { time_secs: 4.5, label: "Ace".into(), kind: "kill".into() },
            CopilotToolCall::AddTransition { clip_id: Some(id.0), transition_kind: "whip_pan".into(), duration_frames: 8 },
            CopilotToolCall::SyncToBeatMarkers { clip_ids: vec![id.0] },
            CopilotToolCall::CutAndRipple { clip_id: Some(id.0), time_secs: 2.0, trim_mode: "blade".into() },
        ];

        for call in &calls {
            let res = execute_tool_call(call, &mut tl, &mut history, &mut playhead, &selection);
            assert!(res.is_ok(), "tool {:?} failed: {:?}", call, res);
        }

        assert!(tl.markers.iter().any(|m| m.name == "Ace"));
        assert!(tl.spine.len() >= 2, "blade split created two spine clips");

        // Every mutation went through `History`, so undoing restores the original single clip.
        while history.can_undo() {
            assert!(history.undo(&mut tl));
        }
        assert_eq!(tl.spine.len(), 1);
        assert_eq!(tl.spine[0].effects.len(), 0);
        assert_eq!(tl.markers.len(), 1);
    }

    #[test]
    fn quick_prompts_work_in_offline_fallback() {
        let mut tl = Timeline::default();
        let asset = MediaAsset::new("clip.mp4", "/clip.mp4", Time::from_secs(6));
        let id = tl.append(Clip::from_asset(&asset)).unwrap();
        let mut history = History::new(64);

        for (label, prompt) in QUICK_PROMPTS {
            let mut tl_copy = tl.clone();
            let mut playhead = Time::from_secs(1);
            let (summary, calls) = offline_copilot_plan(prompt, &tl_copy, playhead, &[id]);
            assert!(!summary.is_empty(), "empty summary for '{label}'");
            assert!(!calls.is_empty(), "no calls generated for '{label}'");
            for call in calls {
                execute_tool_call(&call, &mut tl_copy, &mut history, &mut playhead, &[id]).unwrap();
            }
        }
    }
}
