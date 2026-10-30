//! STT (speech-to-text) Settings tab: provider switch + GigaAM/Whisper field
//! saves + the live connection test (P1 of `docs/overlay-host-gaps-and-next-checks.md`
//! — splitting the `settings_controller.rs` god-function by domain, the same way
//! Phase 2's `diagnostics.rs` and Wave 1's `settings_vision.rs` were extracted).
//!
//! This module owns the STT wiring previously inlined in `open_settings`: the
//! GigaAM GPU toggle (`on_stt_gigaam_gpu_changed` — which sat up in the AI-local
//! block region, not contiguous with the rest), the Groq/local connection test
//! (`on_stt_test_clicked`), the provider dropdown (`on_stt_provider_changed`),
//! the GigaAM model-dir save, and the Whisper url / bearer / model saves. The
//! blocks moved here VERBATIM — same captures (`cfg.clone()`, plus `win.as_weak()`
//! for the test), same bodies, byte-for-byte identical behavior. `open_settings`
//! now only CALLS `wire_stt_settings(&win, cfg)` where the main STT cluster was.
//!
//! NOT moved (different domain, left in `open_settings`): the Audio-device mic
//! callbacks `on_mic_device_selected` / `on_mic_test_clicked`.
//!
//! SECURITY (unchanged by this mechanical move): the STT test-result tile keeps
//! its GENERIC messages (`[ok] …` / `[err] …`, error chain capped at 90 chars) so
//! no `base_url` / LAN IP leaks into a screen-shared Settings window. The endpoint
//! resolves via `cfg.stt_backend()` exactly as before.
//!
//! NOTE: this extraction imports the parent crate-root via `use super::*;`
//! (reaching `SettingsWindow` / `SharedString` / the `diag!` macro / the
//! `overlay_backend` config + stt helpers). That is intentional for the move;
//! imports narrow in a later pass.
use super::{ComponentHandle, SettingsWindow, SharedString};
use slint::{Model, ModelRc, VecModel};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

/// The two Groq cloud model ids exposed in the STT tab combobox.
/// Index 0 = turbo (fast, recommended), index 1 = large-v3 (more accurate).
pub(crate) const CLOUD_MODELS: [&str; 2] = ["whisper-large-v3-turbo", "whisper-large-v3"];

/// Map a cloud-model combobox index to the Groq model id.
/// Unknown indices fall back to the recommended turbo model.
pub(crate) fn cloud_model_from_index(idx: i32) -> &'static str {
    match idx {
        1 => CLOUD_MODELS[1],
        _ => CLOUD_MODELS[0],
    }
}

/// Map a persisted Groq model id back to the combobox index.
/// Only the two supported values are matched; anything else (empty,
/// legacy, hand-edited) maps to 0 so the UI shows a valid selection.
pub(crate) fn cloud_model_index(model: &str) -> i32 {
    match model {
        "whisper-large-v3" => 1,
        _ => 0,
    }
}

/// Map the persisted provider to the shared dropdown index.
pub(crate) fn stt_provider_index(provider: &str) -> i32 {
    match provider {
        "gigaam" => 1,
        "whisper" => 2,
        "speechmatics" => 3,
        "elevenlabs" => 4,
        "deepgram" => 5,
        _ => 0,
    }
}

/// Map the platform-specific dropdown index back to a persisted provider.
pub(crate) fn stt_provider_from_index(idx: i32) -> &'static str {
    match idx {
        1 => "gigaam",
        2 => "whisper",
        3 => "speechmatics",
        4 => "elevenlabs",
        5 => "deepgram",
        _ => "cloud",
    }
}

/// Resolve a fallback selector (0=off; remaining indices are primary + 1).
fn fallback_index(id: &str) -> i32 {
    if matches!(id, "cloud" | "gigaam" | "whisper" | "speechmatics" | "elevenlabs" | "deepgram") {
        stt_provider_index(id) + 1
    } else { 0 }
}

pub(crate) fn show_stt_fallbacks(win: &SettingsWindow, cfg: &overlay_backend::config::Config) {
    let slots = &cfg.stt_fallbacks;
    let idx = |n: usize| fallback_index(slots.get(n).map(String::as_str).unwrap_or(""));
    win.set_stt_fallback_1_index(idx(0));
    win.set_stt_fallback_2_index(idx(1));
    win.set_stt_fallback_3_index(idx(2));
    win.set_stt_fallback_4_index(idx(3));
    win.set_stt_fallback_5_index(idx(4));
}

fn managed_gigaam_dir() -> PathBuf {
    overlay_backend::local_ai::gigaam_default_dir(&overlay_backend::local_ai::default_root())
}

pub(crate) fn installed_gigaam_dir(saved: &str) -> Option<PathBuf> {
    let saved = Path::new(saved.trim());
    if !saved.as_os_str().is_empty() && overlay_backend::local_ai::gigaam_model_present(saved) {
        return Some(saved.to_path_buf());
    }
    let managed = managed_gigaam_dir();
    overlay_backend::local_ai::gigaam_model_present(&managed).then_some(managed)
}

fn format_mebibytes(bytes: u64) -> String {
    format!("{:.0}", bytes as f64 / 1_048_576.0)
}

fn update_cloud_model<E>(
    config: &mut overlay_backend::config::Config,
    idx: i32,
    save: impl FnOnce(&overlay_backend::config::Config) -> Result<(), E>,
) -> Result<(), E> {
    let previous = std::mem::replace(
        &mut config.stt_model,
        cloud_model_from_index(idx).to_string(),
    );
    if let Err(error) = save(config) {
        config.stt_model = previous;
        return Err(error);
    }
    Ok(())
}

/// Wire the STT-tab Settings callbacks onto the Settings window. Moved VERBATIM
/// out of `open_settings` (P1 domain split) — same captures, same behavior.
/// Needs only `win` (for the closures + the test's `as_weak()`) and `cfg`
/// (cloned per closure); none of the STT blocks touch `state` / `overlay_weak`
/// / `slint_rt` / `rt_handle`, so no extra params are threaded through.
pub(crate) fn wire_stt_settings(win: &SettingsWindow, cfg: &overlay_backend::config::SharedConfig) {
    let gigaam_cancel = Arc::new(AtomicBool::new(false));
    {
        let weak = win.as_weak();
        let cfg_c = cfg.clone();
        let cancel = gigaam_cancel.clone();
        win.on_stt_gigaam_install(move || {
            let Some(w) = weak.upgrade() else { return };
            cancel.store(false, Ordering::Release);
            w.set_stt_gigaam_installing(true);
            w.set_stt_gigaam_install_failed(false);
            w.set_stt_gigaam_install_cancelled(false);
            w.set_stt_gigaam_install_progress(0.0);
            let weak_done = w.as_weak();
            let cfg_t = cfg_c.clone();
            let cancel_t = cancel.clone();
            std::thread::spawn(move || {
                let options = overlay_backend::local_ai::InstallOptions {
                    skip_llama: true,
                    skip_whisper: true,
                    ..Default::default()
                };
                let weak_progress = weak_done.clone();
                let result = overlay_backend::local_ai::install(
                    &options,
                    &cancel_t,
                    &move |progress| {
                        let overlay_backend::local_ai::Progress::Bytes { done, total, .. } =
                            progress
                        else {
                            return;
                        };
                        let weak = weak_progress.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.set_stt_gigaam_install_progress(
                                    done as f32 / total.max(1) as f32,
                                );
                                w.set_stt_gigaam_install_done(SharedString::from(
                                    format_mebibytes(done),
                                ));
                                w.set_stt_gigaam_install_total(SharedString::from(
                                    format_mebibytes(total),
                                ));
                            }
                        });
                    },
                )
                .and_then(|result| {
                    if result.stt_gigaam_dir.is_empty() {
                        anyhow::bail!("managed GigaAM install did not produce a ready model");
                    }
                    let mut c = cfg_t.write();
                    c.stt_provider = "gigaam".into();
                    c.stt_gigaam_dir = result.stt_gigaam_dir.clone();
                    overlay_backend::stt::reset_gigaam_cache();
                    overlay_backend::config::save(&c)?;
                    Ok(result.stt_gigaam_dir)
                });
                let cancelled = cancel_t.load(Ordering::Acquire);
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(w) = weak_done.upgrade() else { return };
                    w.set_stt_gigaam_installing(false);
                    w.set_stt_gigaam_install_cancelled(cancelled);
                    w.set_stt_gigaam_install_failed(result.is_err() && !cancelled);
                    if let Ok(dir) = result {
                        w.set_stt_gigaam_installed(true);
                        w.set_stt_gigaam_dir_input(SharedString::from(dir));
                        w.set_stt_provider_index(1);
                    }
                });
            });
        });
    }
    {
        let cancel = gigaam_cancel;
        win.on_stt_gigaam_install_cancel(move || cancel.store(true, Ordering::Release));
    }
    {
        let cfg_c = cfg.clone();
        win.on_stt_gigaam_gpu_changed(move |on| {
            let mut c = cfg_c.write();
            c.stt_gigaam_gpu = on;
            let _ = overlay_backend::config::save(&c);
            // Apply immediately for the next model load. An active live session
            // keeps its current shared handle until that session stops.
            overlay_backend::stt::configure_gigaam_accelerator(on);
            overlay_backend::stt::reset_gigaam_cache();
        });
    }

    // Phase E6 v27 — STT (Groq) connection test. Same off-thread
    // pattern; hits the Groq /models endpoint with the saved key.
    {
        let cfg_c = cfg.clone();
        let weak = win.as_weak();
        win.on_stt_test_clicked(move || {
            let Some(w) = weak.upgrade() else { return };
            w.set_stt_test_result(SharedString::from("testing…"));
            let backend = cfg_c.read().stt_backend();
            let weak_res = w.as_weak();
            std::thread::spawn(move || {
                let msg = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => {
                        match rt.block_on(overlay_backend::stt::test_connection_backend(&backend)) {
                            Ok(s) => format!("[ok] {s}"),
                            Err(e) => format!("[err] {e:#}").chars().take(90).collect(),
                        }
                    }
                    Err(e) => format!("[err] runtime: {e}"),
                };
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak_res.upgrade() {
                        w.set_stt_test_result(SharedString::from(msg));
                    }
                });
            });
        });
    }

    // Phase E10 — STT provider selector + local-engine fields.
    {
        let cfg_c = cfg.clone();
        let weak = win.as_weak();
        win.on_stt_provider_changed(move |idx| {
            let provider = stt_provider_from_index(idx);
            let mut c = cfg_c.write();
            c.stt_provider = provider.to_string();
            for name in &mut c.stt_fallbacks {
                if name == provider { name.clear(); }
            }
            if let Some(w) = weak.upgrade() { show_stt_fallbacks(&w, &c); }
            if provider == "gigaam" {
                if let Some(dir) = installed_gigaam_dir(&c.stt_gigaam_dir) {
                    c.stt_gigaam_dir = dir.to_string_lossy().into_owned();
                    if let Some(w) = weak.upgrade() {
                        w.set_stt_gigaam_dir_input(SharedString::from(
                            c.stt_gigaam_dir.clone(),
                        ));
                        w.set_stt_gigaam_installed(true);
                    }
                }
            }
            overlay_backend::stt::reset_gigaam_cache();
            if let Err(e) = overlay_backend::config::save(&c) {
                eprintln!("[overlay-host] stt_provider save failed: {e:#}");
                return;
            }
            diag!("stt_provider -> {provider}");
            drop(c);
            if let Some(w) = weak.upgrade() { refresh_active_stt_models(&w, &cfg_c); }
        });
    }
    // The five reserve slots are stored in config.json in explicit priority
    // order. Selecting a duplicate moves it to the newly chosen position.
    {
        let cfg_c = cfg.clone();
        let weak = win.as_weak();
        win.on_stt_fallback_changed(move |slot, selected_idx| {
            if !(0..5).contains(&slot) || !(0..=6).contains(&selected_idx) { return; }
            let chosen = if selected_idx == 0 { "" }
                else { stt_provider_from_index(selected_idx - 1) };
            let mut c = cfg_c.write();
            let previous = c.stt_fallbacks.clone();
            c.stt_fallbacks.resize(5, String::new());
            if !chosen.is_empty() {
                for (i, name) in c.stt_fallbacks.iter_mut().enumerate() {
                    if i != slot as usize && name == chosen { name.clear(); }
                }
            }
            c.stt_fallbacks[slot as usize] = if chosen == c.stt_provider { "".into() }
                else { chosen.to_string() };
            if let Err(e) = overlay_backend::config::save(&c) {
                c.stt_fallbacks = previous;
                eprintln!("[overlay-host] STT fallback save failed: {e:#}");
            }
            if let Some(w) = weak.upgrade() { show_stt_fallbacks(&w, &c); }
        });
    }
    // New cloud engines keep independent API keys and model identifiers.
    // Never emit secret values into logs or Slint-visible properties.
    {
        let cfg_c = cfg.clone(); let weak = win.as_weak();
        win.on_stt_speechmatics_key_save(move |value| {
            let v = value.trim();
            if v.is_empty() { return; }
            let mut c = cfg_c.write(); c.stt_speechmatics_key = v.to_owned();
            if overlay_backend::config::save(&c).is_ok() {
                if let Some(w) = weak.upgrade() { w.set_stt_speechmatics_key_status("[ok] stored".into()); }
                drop(c);
                if cfg_c.read().stt_provider == "speechmatics" {
                    if let Some(w) = weak.upgrade() { refresh_active_stt_models(&w, &cfg_c); }
                }
            }
        });
    }
    {
        let cfg_c = cfg.clone(); let weak = win.as_weak();
        win.on_stt_elevenlabs_key_save(move |value| {
            let v = value.trim();
            if v.is_empty() { return; }
            let mut c = cfg_c.write(); c.stt_elevenlabs_key = v.to_owned();
            if overlay_backend::config::save(&c).is_ok() {
                if let Some(w) = weak.upgrade() { w.set_stt_elevenlabs_key_status("[ok] stored".into()); }
                drop(c);
                if cfg_c.read().stt_provider == "elevenlabs" {
                    if let Some(w) = weak.upgrade() { refresh_active_stt_models(&w, &cfg_c); }
                }
            }
        });
    }
    {
        let cfg_c = cfg.clone(); let weak = win.as_weak();
        win.on_stt_deepgram_key_save(move |value| {
            let v = value.trim();
            if v.is_empty() { return; }
            let mut c = cfg_c.write(); c.stt_deepgram_key = v.to_owned();
            if overlay_backend::config::save(&c).is_ok() {
                if let Some(w) = weak.upgrade() { w.set_stt_deepgram_key_status("[ok] stored".into()); }
                drop(c);
                if cfg_c.read().stt_provider == "deepgram" {
                    if let Some(w) = weak.upgrade() { refresh_active_stt_models(&w, &cfg_c); }
                }
            }
        });
    }
    {
        let cfg_c = cfg.clone();
        win.on_stt_speechmatics_model_save(move |value| {
            let v = value.trim(); if v.is_empty() { return; }
            let mut c = cfg_c.write(); c.stt_speechmatics_model = v.to_owned();
            let _ = overlay_backend::config::save(&c);
        });
    }
    {
        let cfg_c = cfg.clone();
        win.on_stt_elevenlabs_model_save(move |value| {
            let v = value.trim(); if v.is_empty() { return; }
            let mut c = cfg_c.write(); c.stt_elevenlabs_model = v.to_owned();
            let _ = overlay_backend::config::save(&c);
        });
    }
    {
        let cfg_c = cfg.clone();
        win.on_stt_deepgram_model_save(move |value| {
            let v = value.trim(); if v.is_empty() { return; }
            let mut c = cfg_c.write(); c.stt_deepgram_model = v.to_owned();
            let _ = overlay_backend::config::save(&c);
        });
    }

    // All model combo selections persist immediately, like the Groq model picker.
    {
        let weak = win.as_weak(); let cfg_c = cfg.clone();
        win.on_stt_speechmatics_model_changed(move |index| {
            let Some(w) = weak.upgrade() else { return; };
            if index < 0 { return; }
            let Some(model) = w.get_stt_speechmatics_models().row_data(index as usize) else { return; };
            let previous = cfg_c.read().stt_speechmatics_model.clone();
            let mut c = cfg_c.write(); c.stt_speechmatics_model = model.to_string();
            if let Err(e) = overlay_backend::config::save(&c) {
                c.stt_speechmatics_model = previous.clone();
                eprintln!("[overlay-host] speechmatics model save failed: {e:#}");
                drop(c);
                let choices = w.get_stt_speechmatics_models();
                let old_index = choices.iter().position(|m| m.as_str() == previous.as_str()).unwrap_or(0);
                w.set_stt_speechmatics_model_index(old_index as i32);
            }
        });
    }
    {
        let weak = win.as_weak(); let cfg_c = cfg.clone();
        win.on_stt_speechmatics_models_refresh(move || {
            if let Some(w) = weak.upgrade() { refresh_stt_models(&w, &cfg_c, "speechmatics"); }
        });
    }
    {
        let weak = win.as_weak(); let cfg_c = cfg.clone();
        win.on_stt_elevenlabs_model_changed(move |index| {
            let Some(w) = weak.upgrade() else { return; };
            if index < 0 { return; }
            let Some(model) = w.get_stt_elevenlabs_models().row_data(index as usize) else { return; };
            let previous = cfg_c.read().stt_elevenlabs_model.clone();
            let mut c = cfg_c.write(); c.stt_elevenlabs_model = model.to_string();
            if let Err(e) = overlay_backend::config::save(&c) {
                c.stt_elevenlabs_model = previous.clone();
                eprintln!("[overlay-host] elevenlabs model save failed: {e:#}");
                drop(c);
                let choices = w.get_stt_elevenlabs_models();
                let old_index = choices.iter().position(|m| m.as_str() == previous.as_str()).unwrap_or(0);
                w.set_stt_elevenlabs_model_index(old_index as i32);
            }
        });
    }
    {
        let weak = win.as_weak(); let cfg_c = cfg.clone();
        win.on_stt_elevenlabs_models_refresh(move || {
            if let Some(w) = weak.upgrade() { refresh_stt_models(&w, &cfg_c, "elevenlabs"); }
        });
    }
    {
        let weak = win.as_weak(); let cfg_c = cfg.clone();
        win.on_stt_deepgram_model_changed(move |index| {
            let Some(w) = weak.upgrade() else { return; };
            if index < 0 { return; }
            let Some(model) = w.get_stt_deepgram_models().row_data(index as usize) else { return; };
            let previous = cfg_c.read().stt_deepgram_model.clone();
            let mut c = cfg_c.write(); c.stt_deepgram_model = model.to_string();
            if let Err(e) = overlay_backend::config::save(&c) {
                c.stt_deepgram_model = previous.clone();
                eprintln!("[overlay-host] deepgram model save failed: {e:#}");
                drop(c);
                let choices = w.get_stt_deepgram_models();
                let old_index = choices.iter().position(|m| m.as_str() == previous.as_str()).unwrap_or(0);
                w.set_stt_deepgram_model_index(old_index as i32);
            }
        });
    }
    {
        let weak = win.as_weak(); let cfg_c = cfg.clone();
        win.on_stt_deepgram_models_refresh(move || {
            if let Some(w) = weak.upgrade() { refresh_stt_models(&w, &cfg_c, "deepgram"); }
        });
    }
    // Cloud recognition model (stt_model): 0=turbo (fast), 1=large-v3 (accurate).
    // Handy-style rollback: on save failure the in-memory config value AND the
    // visible combobox selection revert to the previous state.
    {
        let cfg_c = cfg.clone();
        let weak = win.as_weak();
        win.on_stt_cloud_model_changed(move |idx| {
            let mut c = cfg_c.write();
            if let Err(e) = update_cloud_model(&mut c, idx, overlay_backend::config::save) {
                let prev_idx = cloud_model_index(&c.stt_model);
                drop(c);
                eprintln!("[overlay-host] stt_model save failed: {e:#}");
                if let Some(w) = weak.upgrade() {
                    w.set_stt_cloud_model_index(prev_idx);
                }
                return;
            }
            diag!("stt_model -> {}", cloud_model_from_index(idx));
        });
    }
    {
        // Recognition language (stt_language): 0=auto, 1=ru, 2=en. None = let the
        // engine auto-detect per phrase (Whisper/Groq); a forced language pins it.
        let cfg_c = cfg.clone();
        win.on_stt_language_changed(move |idx| {
            let lang = match idx {
                1 => Some("ru".to_string()),
                2 => Some("en".to_string()),
                _ => None,
            };
            let mut c = cfg_c.write();
            c.stt_language = lang.clone();
            if let Err(e) = overlay_backend::config::save(&c) {
                eprintln!("[overlay-host] stt_language save failed: {e:#}");
                return;
            }
            diag!("stt_language -> {}", lang.as_deref().unwrap_or("auto"));
        });
    }
    {
        let cfg_c = cfg.clone();
        win.on_stt_gigaam_dir_save(move |v| {
            let trimmed = v.trim().to_string();
            let mut c = cfg_c.write();
            c.stt_gigaam_dir = trimmed.clone();
            overlay_backend::stt::reset_gigaam_cache();
            if let Err(e) = overlay_backend::config::save(&c) {
                eprintln!("[overlay-host] stt_gigaam_dir save failed: {e:#}");
                return;
            }
            diag!("stt_gigaam_dir saved ({} chars)", trimmed.len());
        });
    }
    {
        let cfg_c = cfg.clone();
        win.on_stt_whisper_url_save(move |v| {
            let trimmed = v.trim().to_string();
            let mut c = cfg_c.write();
            c.stt_whisper_url = trimmed.clone();
            if let Err(e) = overlay_backend::config::save(&c) {
                eprintln!("[overlay-host] stt_whisper_url save failed: {e:#}");
                return;
            }
            diag!("stt_whisper_url saved ({} chars)", trimmed.len());
        });
    }
    {
        let cfg_c = cfg.clone();
        win.on_stt_whisper_bearer_save(move |v| {
            let trimmed = v.trim().to_string();
            let mut c = cfg_c.write();
            c.stt_whisper_bearer = trimmed.clone();
            if let Err(e) = overlay_backend::config::save(&c) {
                eprintln!("[overlay-host] stt_whisper_bearer save failed: {e:#}");
                return;
            }
            diag!("stt_whisper_bearer saved ({} chars)", trimmed.len());
        });
    }
    {
        let cfg_c = cfg.clone();
        win.on_stt_whisper_model_save(move |v| {
            let trimmed = v.trim().to_string();
            let mut c = cfg_c.write();
            c.stt_whisper_model = trimmed.clone();
            if let Err(e) = overlay_backend::config::save(&c) {
                eprintln!("[overlay-host] stt_whisper_model save failed: {e:#}");
                return;
            }
            diag!("stt_whisper_model saved ({} chars)", trimmed.len());
        });
    }
}


/// Fetch model choices asynchronously; never place API keys into Slint properties.
/// A failed refresh preserves the saved model and currently displayed choices.
pub(crate) fn refresh_active_stt_models(
    win: &SettingsWindow, cfg: &overlay_backend::config::SharedConfig,
) {
    let provider = cfg.read().stt_provider.clone();
    if matches!(provider.as_str(), "speechmatics" | "elevenlabs" | "deepgram") {
        refresh_stt_models(win, cfg, &provider);
    }
}

fn refresh_stt_models(
    win: &SettingsWindow, cfg: &overlay_backend::config::SharedConfig, provider: &str,
) {
    let (key, saved) = {
        let c = cfg.read();
        match provider {
            "speechmatics" => (c.stt_speechmatics_key.clone(), c.stt_speechmatics_model.clone()),
            "elevenlabs" => (c.stt_elevenlabs_key.clone(), c.stt_elevenlabs_model.clone()),
            "deepgram" => (c.stt_deepgram_key.clone(), c.stt_deepgram_model.clone()),
            _ => return,
        }
    };
    match provider {
        "speechmatics" => win.set_stt_speechmatics_models_status("Загрузка моделей...".into()),
        "elevenlabs" => win.set_stt_elevenlabs_models_status("Загрузка моделей...".into()),
        "deepgram" => win.set_stt_deepgram_models_status("Загрузка моделей...".into()),
        _ => {}
    }
    let provider = provider.to_string();
    let weak = win.as_weak();
    std::thread::spawn(move || {
        let result = tokio::runtime::Builder::new_current_thread().enable_all().build()
            .map_err(anyhow::Error::from)
            .and_then(|rt| rt.block_on(overlay_backend::stt::fetch_stt_model_catalog(&provider, &key)));
        let _ = slint::invoke_from_event_loop(move || {
            let Some(w) = weak.upgrade() else { return; };
            let (list, selected, status) = match result {
                Ok(catalog) => {
                    let mut list = catalog.ids;
                    // Preserve a previously selected model even if provider's public list
                    // does not include account-specific or retired models.
                    let in_catalog = list.contains(&saved);
                    if !saved.is_empty() && !in_catalog { list.insert(0, saved.clone()); }
                    let index = list.iter().position(|m| m == &saved).unwrap_or(0) as i32;
                    let status = if !saved.is_empty() && !in_catalog {
                        format!("{}; сохранённая модель отсутствует в каталоге", catalog.source)
                    } else { catalog.source };
                    (Some(list), index, status)
                }
                Err(e) => (None, 0, format!("Не удалось загрузить модели: {e:#}").chars().take(140).collect()),
            };
            match provider.as_str() {
                "speechmatics" => {
                    if let Some(list) = list {
                        w.set_stt_speechmatics_models(ModelRc::new(VecModel::from(list.into_iter().map(SharedString::from).collect::<Vec<_>>() )));
                        w.set_stt_speechmatics_model_index(selected);
                    }
                    w.set_stt_speechmatics_models_status(SharedString::from(status));
                }
                "elevenlabs" => {
                    if let Some(list) = list {
                        w.set_stt_elevenlabs_models(ModelRc::new(VecModel::from(list.into_iter().map(SharedString::from).collect::<Vec<_>>() )));
                        w.set_stt_elevenlabs_model_index(selected);
                    }
                    w.set_stt_elevenlabs_models_status(SharedString::from(status));
                }
                "deepgram" => {
                    if let Some(list) = list {
                        w.set_stt_deepgram_models(ModelRc::new(VecModel::from(list.into_iter().map(SharedString::from).collect::<Vec<_>>() )));
                        w.set_stt_deepgram_model_index(selected);
                    }
                    w.set_stt_deepgram_models_status(SharedString::from(status));
                }
                _ => {}
            }
        });
    });
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn cloud_model_mapping_is_bounded() {
        for (idx, model) in CLOUD_MODELS.iter().enumerate() {
            assert_eq!(cloud_model_from_index(idx as i32), *model);
            assert_eq!(cloud_model_index(model), idx as i32);
        }
        assert_eq!(cloud_model_from_index(-1), "whisper-large-v3-turbo");
        assert_eq!(cloud_model_from_index(2), "whisper-large-v3-turbo");
        assert_eq!(cloud_model_index(""), 0);
        assert_eq!(cloud_model_index("bogus"), 0);
    }

    #[test]
    fn provider_mapping_is_shared_by_windows_and_macos() {
        assert_eq!(stt_provider_index("cloud"), 0);
        assert_eq!(stt_provider_index("gigaam"), 1);
        assert_eq!(stt_provider_index("whisper"), 2);
        assert_eq!(stt_provider_index("speechmatics"), 3);
        assert_eq!(stt_provider_index("elevenlabs"), 4);
        assert_eq!(stt_provider_index("deepgram"), 5);
        assert_eq!(stt_provider_index("retired-provider"), 0);
        assert_eq!(stt_provider_from_index(0), "cloud");
        assert_eq!(stt_provider_from_index(1), "gigaam");
        assert_eq!(stt_provider_from_index(2), "whisper");
        assert_eq!(stt_provider_from_index(3), "speechmatics");
        assert_eq!(stt_provider_from_index(4), "elevenlabs");
        assert_eq!(stt_provider_from_index(5), "deepgram");
    }

    #[test]
    fn gigaam_progress_matches_the_windows_megabyte_display() {
        assert_eq!(format_mebibytes(0), "0");
        assert_eq!(format_mebibytes(224_893_347), "214");
    }

    #[test]
    fn failed_cloud_model_save_rolls_back() {
        let mut config = overlay_backend::config::Config {
            stt_model: "whisper-large-v3-turbo".into(),
            ..Default::default()
        };

        let result = update_cloud_model(&mut config, 1, |_| Err("disk full"));

        assert_eq!(result, Err("disk full"));
        assert_eq!(config.stt_model, "whisper-large-v3-turbo");
    }
}
