//! Crash isolation for Skia on Windows. No tray, hotkeys, audio, or Win32 hooks.
//! The real Suflyor .slint components are loaded through its normal build.rs.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(windows)]
mod ui {
    slint::include_modules!();
}

#[cfg(windows)]
use slint::ComponentHandle;

#[cfg(windows)]
fn show_and_trace<C: ComponentHandle>(window: C) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("[skia-isolate] BEFORE window.show()");
    window.show()?;
    eprintln!("[skia-isolate] AFTER window.show()");

    // Unlike ComponentHandle::run(), this loop remains active until our timer
    // explicitly quits. Avoid relying on window-close behaviour across renderers.
    slint::Timer::single_shot(std::time::Duration::from_secs(5), || {
        eprintln!("[skia-isolate] timeout: requesting clean event-loop exit");
        let _ = slint::quit_event_loop();
    });
    eprintln!("[skia-isolate] BEFORE event loop");
    slint::run_event_loop_until_quit()?;
    eprintln!("[skia-isolate] AFTER event loop");
    window.hide()?;
    eprintln!("[skia-isolate] CLEAN EXIT");
    Ok(())
}

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "spike".to_owned());
    let backend = std::env::var("SLINT_BACKEND").unwrap_or_else(|_| "<unset>".to_owned());
    eprintln!("[skia-isolate] mode={mode}, SLINT_BACKEND={backend}");
    match mode.as_str() {
        "spike" => {
            eprintln!("[skia-isolate] BEFORE OverlaySpike::new()");
            let window = ui::OverlaySpike::new()?;
            eprintln!("[skia-isolate] AFTER OverlaySpike::new()");
            show_and_trace(window)
        }
        "bar" => {
            eprintln!("[skia-isolate] BEFORE OverlayBarWindow::new()");
            let window = ui::OverlayBarWindow::new()?;
            eprintln!("[skia-isolate] AFTER OverlayBarWindow::new()");
            show_and_trace(window)
        }
        _ => Err("Unknown mode: use 'spike' or 'bar'".into()),
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("suflyor-skia-isolate is Windows-only");
}
