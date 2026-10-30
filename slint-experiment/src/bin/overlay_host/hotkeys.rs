//! Global-hotkey registration + the hotkey-registration diagnostics state
//! (Phase 3 of the `overlay_host.rs` modularization — see
//! `docs/overlay-host-modularization-plan.md` §5.3).
//!
//! This module owns the one-time global-hotkey REGISTRATION ([`register_hotkeys`])
//! — it builds the process-wide `GlobalHotKeyManager`, registers the user's
//! configured bindings (defaults mirror F1/F3/F4/F6/Shift+F6/F7/F8/F9), logs each
//! "Fx hotkey registered", and records the per-key outcome into [`HOTKEY_DIAG`]
//! so the Settings ▸ Diagnostics tab can surface a CONFLICTING key by name
//! instead of a blanket "hotkeys disabled" (audit P1.2). The captured outcome is
//! read back through [`hotkey_diag_row`], which `diagnostics.rs` reaches via the
//! parent glob (`use hotkeys::*;` re-exports it at crate root).
//!
//! What deliberately STAYS in `overlay_host.rs`: the hotkey EVENT-DISPATCH timer
//! (the `GlobalHotKeyEvent::receiver()` poll loop), because it captures a dozen
//! main-scoped closures + `Rc`-borrowed window/state slots and only needs to
//! compare the pressed `event.id` against the ids this module hands back. So
//! [`register_hotkeys`] returns a [`RegisteredHotkeys`] carrying the manager
//! (which `main` must keep ALIVE — dropping it unregisters every hotkey) plus
//! each key's `.id()` for the dispatch to match on. Registration order, the log
//! strings, and the `HOTKEY_DIAG` write are preserved byte-for-byte (MECHANICAL
//! move, §2 — no behaviour change).
//!
//! NOTE (§7): this module is self-contained on `global_hotkey` + std (it
//! references NO parent crate-root symbol via short name — everything is a full
//! path), so it carries NO `use super::…` import. It is still the re-export seam
//! (`use hotkeys::*;` in the parent lifts `hotkey_diag_row` to crate root for
//! `diagnostics.rs`).

/// P1.2 — outcome of registering the global hotkeys at startup, captured ONCE so
/// the Diagnostics tab can surface a per-key conflict (not just an eprintln!).
/// Set in the registration loop; read by [`hotkey_diag_row`].
#[allow(dead_code)]
pub(crate) struct HotkeyDiag {
    /// Space-separated keys that registered, e.g. "F1 F3 F4 F6 F8 F9 Shift+F9" (Shift+F9 toggles AI mode).
    registered: String,
    /// Comma-separated keys whose `register()` failed (conflict / already taken).
    /// Empty when every key registered.
    failed: String,
    /// True when the hotkey MANAGER itself couldn't be created (nothing tried).
    manager_missing: bool,
}

static HOTKEY_DIAG: std::sync::OnceLock<HotkeyDiag> = std::sync::OnceLock::new();

/// Render the captured hotkey-registration outcome (P1.2) into a (level, detail,
/// failed) triple for the Diagnostics tab. level 0=all registered, 4=a key
/// failed (conflict), 3=manager unavailable / not yet captured. `failed` is the
/// raw key list so the .slint composes the translated "conflict:" wording.
#[allow(dead_code)]
pub(crate) fn hotkey_diag_row() -> (i32, String, String) {
    match HOTKEY_DIAG.get() {
        None => (3, String::new(), String::new()),
        // Manager failed to init → level 4 (error) but EMPTY failed list, so the
        // UI/report render "unavailable", not a misleading "conflict: all".
        Some(d) if d.manager_missing => (4, String::new(), String::new()),
        Some(d) if d.failed.is_empty() => (0, d.registered.clone(), String::new()),
        Some(d) => (4, d.registered.clone(), d.failed.clone()),
    }
}

/// Result of [`register_hotkeys`]: the live `GlobalHotKeyManager` (which the
/// caller MUST keep alive — dropping it unregisters every hotkey) plus each
/// registered key's `.id()` so the dispatch timer in `main` can match a pressed
/// `GlobalHotKeyEvent` against the right action. `manager` is `None` when the
/// manager itself couldn't be created (hotkeys disabled); the ids are still
/// computed (they're just never delivered, so the dispatch arms stay dormant).
#[allow(dead_code)]
pub(crate) struct RegisteredHotkeys {
    /// Kept alive by `main`; `None` when the manager couldn't be created.
    pub manager: Option<global_hotkey::GlobalHotKeyManager>,
    pub f1_id: u32,
    pub f3_id: u32,
    pub f4_id: u32,
    pub f6_id: u32,
    pub close_all_id: u32,
    pub f7_id: u32,
    pub f8_id: u32,
    pub sf8_id: u32,
    /// Ctrl+F8 — region capture in OCR / read-aloud mode (read-aloud feature).
    pub cf8_id: u32,
    pub f9_id: u32,
    pub sf9_id: u32,
    /// Shift+Alt+1/2/3 — read selection / OCR region / pause-resume (read-aloud).
    pub sa1_id: u32,
    pub sa2_id: u32,
    pub sa3_id: u32,
}

/// ===== Global hotkeys (Phase D2 + B3 extra) =====
///
/// global-hotkey 0.6 owns a single process-wide event receiver +
/// platform-specific manager. We register one hotkey per F-key,
/// then poll the receiver every 50 ms from a Slint Timer — fires
/// on UI thread so we can touch Rc-borrowed state without Send.
///
/// Default bindings are shown in Settings ▸ Hotkeys and can be edited there.
/// Registration happens once at startup, so saved changes take effect on restart.
///
/// Returns the manager (kept alive by the caller) + each key's id for the
/// dispatch loop. Registration order, the per-key log lines, and the
/// `HOTKEY_DIAG` write are identical to the former inline block in `main`.
fn configured_hotkey(
    configured: &str,
    fallback: &str,
    action: &str,
) -> global_hotkey::hotkey::HotKey {
    match configured.trim().parse::<global_hotkey::hotkey::HotKey>() {
        Ok(hotkey) => hotkey,
        Err(error) => {
            eprintln!(
                "[overlay-host] invalid hotkey for {action}: {configured:?} ({error}); using {fallback}"
            );
            match fallback.parse::<global_hotkey::hotkey::HotKey>() {
                Ok(hotkey) => hotkey,
                Err(_) => global_hotkey::hotkey::HotKey::new(
                    None,
                    global_hotkey::hotkey::Code::F24,
                ),
            }
        }
    }
}

/// Validate all user-editable bindings before persisting them. Besides syntax,
/// duplicate physical hotkeys are rejected because the OS can register only one
/// action for a given combination.
pub(crate) fn hotkey_bindings_valid(
    bindings: &overlay_backend::config::HotkeyBindings,
) -> bool {
    let values = [
        bindings.help.as_str(),
        bindings.reask.as_str(),
        bindings.knowledge.as_str(),
        bindings.new_tile.as_str(),
        bindings.close_all_tiles.as_str(),
        bindings.archive.as_str(),
        bindings.vision.as_str(),
        bindings.vision_translate.as_str(),
        bindings.vision_ocr.as_str(),
        bindings.ask.as_str(),
        bindings.ask_deep.as_str(),
        bindings.read_selected.as_str(),
        bindings.read_ocr.as_str(),
        bindings.read_pause.as_str(),
    ];
    let mut ids = std::collections::HashSet::new();
    for value in values {
        let Ok(hotkey) = value.trim().parse::<global_hotkey::hotkey::HotKey>() else {
            return false;
        };
        if !ids.insert(hotkey.id()) {
            return false;
        }
    }
    true
}

pub(crate) fn register_hotkeys(
    bindings: &overlay_backend::config::HotkeyBindings,
) -> RegisteredHotkeys {
    let hotkey_manager = match global_hotkey::GlobalHotKeyManager::new() {
        Ok(m) => Some(m),
        Err(e) => {
            eprintln!("[overlay-host] GlobalHotKeyManager init failed: {e}. Hotkeys disabled.");
            None
        }
    };
    let defaults = overlay_backend::config::HotkeyBindings::default();
    let f3_hotkey = configured_hotkey(&bindings.reask, &defaults.reask, "reask");
    let f4_hotkey = configured_hotkey(&bindings.knowledge, &defaults.knowledge, "knowledge");
    let f6_hotkey = configured_hotkey(&bindings.new_tile, &defaults.new_tile, "new_tile");
    let close_all_hotkey = configured_hotkey(
        &bindings.close_all_tiles,
        &defaults.close_all_tiles,
        "close_all_tiles",
    );
    let f7_hotkey = configured_hotkey(&bindings.archive, &defaults.archive, "archive");
    let f8_hotkey = configured_hotkey(&bindings.vision, &defaults.vision, "vision");
    let sf8_hotkey = configured_hotkey(
        &bindings.vision_translate,
        &defaults.vision_translate,
        "vision_translate",
    );
    let cf8_hotkey = configured_hotkey(&bindings.vision_ocr, &defaults.vision_ocr, "vision_ocr");
    let f9_hotkey = configured_hotkey(&bindings.ask, &defaults.ask, "ask");
    let sf9_hotkey = configured_hotkey(&bindings.ask_deep, &defaults.ask_deep, "ask_deep");
    let sa1_hotkey = configured_hotkey(
        &bindings.read_selected,
        &defaults.read_selected,
        "read_selected",
    );
    let sa2_hotkey = configured_hotkey(&bindings.read_ocr, &defaults.read_ocr, "read_ocr");
    let sa3_hotkey = configured_hotkey(&bindings.read_pause, &defaults.read_pause, "read_pause");
    let f1_hotkey = configured_hotkey(&bindings.help, &defaults.help, "help");
    let f3_id = f3_hotkey.id();
    let f4_id = f4_hotkey.id();
    let f6_id = f6_hotkey.id();
    let close_all_id = close_all_hotkey.id();
    let f7_id = f7_hotkey.id();
    let f9_id = f9_hotkey.id();
    let sf9_id = sf9_hotkey.id();
    let f8_id = f8_hotkey.id();
    let sf8_id = sf8_hotkey.id();
    let cf8_id = cf8_hotkey.id();
    let f1_id = f1_hotkey.id();
    let sa1_id = sa1_hotkey.id();
    let sa2_id = sa2_hotkey.id();
    let sa3_id = sa3_hotkey.id();
    {
        // P1.2 — capture per-key registration so the Diagnostics tab can name a
        // conflicting key instead of a blanket "hotkeys disabled" (audit P1.2).
        let mut registered: Vec<&str> = Vec::new();
        let mut failed: Vec<&str> = Vec::new();
        if let Some(m) = hotkey_manager.as_ref() {
            for (label, hk) in [
                (bindings.reask.as_str(), f3_hotkey),
                (bindings.knowledge.as_str(), f4_hotkey),
                (bindings.new_tile.as_str(), f6_hotkey),
                (bindings.close_all_tiles.as_str(), close_all_hotkey),
                (bindings.archive.as_str(), f7_hotkey),
                (bindings.vision.as_str(), f8_hotkey),
                (bindings.vision_translate.as_str(), sf8_hotkey),
                (bindings.vision_ocr.as_str(), cf8_hotkey),
                (bindings.ask.as_str(), f9_hotkey),
                (bindings.ask_deep.as_str(), sf9_hotkey),
                (bindings.help.as_str(), f1_hotkey),
                (bindings.read_selected.as_str(), sa1_hotkey),
                (bindings.read_ocr.as_str(), sa2_hotkey),
                (bindings.read_pause.as_str(), sa3_hotkey),
            ] {
                match m.register(hk) {
                    Ok(()) => {
                        eprintln!("[overlay-host] {label} hotkey registered");
                        registered.push(label);
                    }
                    Err(e) => {
                        eprintln!("[overlay-host] {label} register failed: {e}");
                        failed.push(label);
                    }
                }
            }
        }
        let _ = HOTKEY_DIAG.set(HotkeyDiag {
            registered: registered.join(" "),
            failed: failed.join(", "),
            manager_missing: hotkey_manager.is_none(),
        });
    }

    RegisteredHotkeys {
        manager: hotkey_manager,
        f1_id,
        f3_id,
        f4_id,
        f6_id,
        close_all_id,
        f7_id,
        f8_id,
        sf8_id,
        cf8_id,
        f9_id,
        sf9_id,
        sa1_id,
        sa2_id,
        sa3_id,
    }
}
