//! Tray icon (DESIGN Step 15, operator spec 2026-10-25).
//!
//! The tray is a *view*: it renders the scheduler's status snapshot and sends the same
//! scheduler commands the HTTP handlers send — no tray-only state. Colors: green = a
//! model is loaded, yellow = a load/unload in flight, gray = nothing loaded, red = the
//! most recent terminal job ended in an error (message in the tooltip). Left click
//! does nothing; the right-click menu is Open Web UI / Unload All Models / Unload
//! WORK / Unload RP / Unload All AUX / Unload EMB / Exit.
//!
//! The Win32 message pump is the module's documented `unsafe`: one dedicated thread
//! owns a hidden window and pumps messages for the tray-icon crate; the safety
//! invariant is that all tray/menu interactions happen on, or are dispatched by, that
//! single thread (Step 8 convention: contained, justified, documented).

use crate::config::CategoryId;
use crate::scheduler::{SchedulerHandle, SchedulerStatus};
use std::sync::OnceLock;
use tokio::runtime::Handle;
use tokio::sync::watch;
use tray_icon::menu::{MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

/// Icon edge length in pixels (small system tray size).
const ICON_SIZE: u32 = 32;
/// Status refresh cadence.
const REFRESH_MS: u32 = 2_000;
/// `CREATE_NO_WINDOW`: opening the Web UI must not flash a console (defect E5).
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Menu item ids.
mod menu_id {
    pub const OPEN_WEB_UI: &str = "open_web_ui";
    pub const UNLOAD_ALL: &str = "unload_all";
    pub const UNLOAD_WORK: &str = "unload_work";
    pub const UNLOAD_RP: &str = "unload_rp";
    pub const UNLOAD_AUX: &str = "unload_aux";
    pub const UNLOAD_EMB: &str = "unload_emb";
    pub const EXIT: &str = "exit";
}

/// Tray icon state, derived from one scheduler snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayState {
    /// A model is loaded.
    Green,
    /// A load or unload is in flight.
    Yellow,
    /// No model loaded.
    Gray,
    /// An error needs the operator's attention.
    Red,
}

/// Everything the global menu-event handler needs to act.
struct TrayCtx {
    scheduler: SchedulerHandle,
    listeners: crate::listeners::ListenerRegistry,
    control_url: String,
    runtime: Handle,
    shutdown: watch::Sender<bool>,
}

static CTX: OnceLock<TrayCtx> = OnceLock::new();

/// What the operator asked the tray to do (from the right-click menu).
enum Command {
    OpenWebUi,
    UnloadAll,
    Unload(CategoryId),
    Exit,
}

fn command_for(id: &str) -> Option<Command> {
    match id {
        menu_id::OPEN_WEB_UI => Some(Command::OpenWebUi),
        menu_id::UNLOAD_ALL => Some(Command::UnloadAll),
        menu_id::UNLOAD_WORK => Some(Command::Unload(CategoryId::Work)),
        menu_id::UNLOAD_RP => Some(Command::Unload(CategoryId::Rp)),
        menu_id::UNLOAD_AUX => Some(Command::Unload(CategoryId::Aux)),
        menu_id::UNLOAD_EMB => Some(Command::Unload(CategoryId::Emb)),
        menu_id::EXIT => Some(Command::Exit),
        _ => None,
    }
}

/// Spawns the tray thread. Returns nothing; the tray dies with the process.
///
/// # Panics
///
/// Panics on the tray thread (and so fails loudly) only if the icon cannot be built —
/// a broken tray must be visible, not silent.
pub fn spawn(
    scheduler: SchedulerHandle,
    listeners: crate::listeners::ListenerRegistry,
    control_url: String,
    runtime: Handle,
    shutdown: watch::Sender<bool>,
) {
    let ctx = TrayCtx {
        scheduler,
        listeners,
        control_url,
        runtime,
        shutdown,
    };
    std::thread::Builder::new()
        .name("simplehar-tray".to_string())
        .spawn(move || {
            if let Err(error) = run(ctx) {
                tracing::error!(error = %error, "tray unavailable; continuing headless");
            }
        })
        .expect("invariant: spawning a named OS thread does not fail on this platform");
}

fn run(ctx: TrayCtx) -> Result<(), String> {
    let menu = tray_icon::menu::Menu::new();
    let items: Vec<Box<dyn tray_icon::menu::IsMenuItem>> = vec![
        Box::new(MenuItem::with_id(
            menu_id::OPEN_WEB_UI,
            "Open Web UI",
            true,
            None,
        )),
        Box::new(MenuItem::with_id(
            menu_id::UNLOAD_ALL,
            "Unload All Models",
            true,
            None,
        )),
        Box::new(MenuItem::with_id(
            menu_id::UNLOAD_WORK,
            "Unload WORK",
            true,
            None,
        )),
        Box::new(MenuItem::with_id(
            menu_id::UNLOAD_RP,
            "Unload RP",
            true,
            None,
        )),
        Box::new(MenuItem::with_id(
            menu_id::UNLOAD_AUX,
            "Unload All AUX",
            true,
            None,
        )),
        Box::new(MenuItem::with_id(
            menu_id::UNLOAD_EMB,
            "Unload EMB",
            true,
            None,
        )),
        Box::new(PredefinedMenuItem::separator()),
        Box::new(MenuItem::with_id(menu_id::EXIT, "Exit", true, None)),
    ];
    for item in &items {
        menu.append(item.as_ref()).map_err(|e| e.to_string())?;
    }
    let icon = solid_icon(TrayState::Gray)?;
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("SimpleHar")
        .with_icon(icon)
        .build()
        .map_err(|e| format!("cannot create tray icon: {e}"))?;
    CTX.set(ctx)
        .map_err(|_| "tray context already initialized".to_string())?;
    MenuEvent::set_event_handler(Some(|event: MenuEvent| handle_menu(event.id)));
    pump_messages(&tray);
    Ok(())
}

/// Dispatches menu picks: same commands the HTTP handlers send, no tray-only path.
fn handle_menu(id: MenuId) {
    let Some(ctx) = CTX.get() else {
        return;
    };
    let Some(command) = command_for(id.as_ref()) else {
        return;
    };
    match command {
        Command::OpenWebUi => {
            let mut opener = std::process::Command::new("cmd");
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                opener.args(["/C", "start", "", &ctx.control_url]);
                let _ = opener.creation_flags(CREATE_NO_WINDOW).spawn();
            }
            #[cfg(not(windows))]
            {
                let _ = opener.arg(&ctx.control_url).spawn();
            }
        }
        Command::UnloadAll => {
            let _ = ctx.runtime.block_on(ctx.scheduler.unload_all());
        }
        Command::Unload(category) => {
            let _ = ctx.runtime.block_on(ctx.scheduler.unload(category));
        }
        Command::Exit => {
            let _ = ctx.shutdown.send(true);
        }
    }
}

/// Refresh tick: derive the state, update icon and tooltip when they change. The
/// `TrayIcon` is not `Send`/`Sync` on Windows — it lives on, and is only touched by,
/// the tray thread (this function).
fn refresh(tray: &TrayIcon, last_state: &mut Option<TrayState>, last_tooltip: &mut Option<String>) {
    let Some(ctx) = CTX.get() else {
        return;
    };
    let Ok(status) = ctx.runtime.block_on(ctx.scheduler.status()) else {
        return;
    };
    let state = tray_state(&status, ctx.listeners.any_down());
    let tooltip = tooltip(&status);
    if *last_state != Some(state) {
        match solid_icon(state) {
            Ok(icon) => {
                let _ = tray.set_icon(Some(icon));
            }
            Err(error) => tracing::warn!(error = %error, "tray icon render failed"),
        }
        *last_state = Some(state);
    }
    if last_tooltip.as_deref() != Some(tooltip.as_str()) {
        let _ = tray.set_tooltip(Some(&tooltip));
        *last_tooltip = Some(tooltip);
    }
}

/// The tray state for one snapshot; precedence red > yellow > green > gray (Step 15).
///
/// Red is harness-level degradation (operator, 2026-10-25): a listener that cannot
/// bind or died and cannot be restarted. Per-job errors are *not* red — they do not
/// break the app; they ride in the tooltip.
pub fn tray_state(status: &SchedulerStatus, any_listener_down: bool) -> TrayState {
    let in_flight = status.jobs.iter().any(|job| !job.terminal);
    let loaded = status.slots.iter().any(|slot| slot.model.is_some());
    if any_listener_down {
        TrayState::Red
    } else if in_flight {
        TrayState::Yellow
    } else if loaded {
        TrayState::Green
    } else {
        TrayState::Gray
    }
}

/// Hover text: one line with every category's occupants; the error line when red.
pub fn tooltip(status: &SchedulerStatus) -> String {
    let mut line = String::from("SimpleHar — ");
    for (index, category) in CategoryId::all().iter().enumerate() {
        if index > 0 {
            line.push_str(" | ");
        }
        let models: Vec<String> = status
            .slots
            .iter()
            .filter(|slot| slot.category == *category)
            .filter_map(|slot| slot.model.clone())
            .collect();
        if models.is_empty() {
            line.push_str(&format!("{category}: —"));
        } else {
            line.push_str(&format!("{category}: {}", models.join(", ")));
        }
    }
    if let Some(fault) = crate::scheduler::headline_fault(&status.jobs) {
        if let Some(error) = &fault.error {
            line.push_str(&format!("\n{error}"));
        }
    }
    line
}

/// Renders a filled-circle icon of `state`'s color; no asset files (Step 15).
fn solid_icon(state: TrayState) -> Result<Icon, String> {
    let (red, green, blue): (u8, u8, u8) = match state {
        TrayState::Green => (0x2e, 0xa0, 0x43),
        TrayState::Yellow => (0xd8, 0x9b, 0x1d),
        TrayState::Gray => (0x8a, 0x8f, 0x98),
        TrayState::Red => (0xc0, 0x39, 0x2b),
    };
    let size = ICON_SIZE as i32;
    let center = size as f32 / 2.0;
    let radius = center - 2.0;
    let mut rgba = Vec::with_capacity((ICON_SIZE * ICON_SIZE * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let distance =
                ((x as f32 - center + 0.5).powi(2) + (y as f32 - center + 0.5).powi(2)).sqrt();
            if distance <= radius {
                rgba.extend_from_slice(&[red, green, blue, 0xff]);
            } else {
                rgba.extend_from_slice(&[0, 0, 0, 0]);
            }
        }
    }
    Icon::from_rgba(rgba, ICON_SIZE, ICON_SIZE).map_err(|e| e.to_string())
}

/// Win32 message pump: waits for messages or the refresh tick, whichever comes first.
///
/// # Safety
///
/// Single-threaded by construction: this runs on the dedicated tray thread that owns
/// the (hidden) window and the tray icon; all tray-icon interactions originate here or
/// in the menu handler dispatched by this loop.
fn pump_messages(tray: &TrayIcon) {
    #[cfg(windows)]
    unsafe {
        use windows::core::w;
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DispatchMessageW, MsgWaitForMultipleObjectsEx,
            PeekMessageW, RegisterClassW, TranslateMessage, MSG, MWMO_INPUTAVAILABLE, PM_REMOVE,
            QS_ALLINPUT, WINDOW_EX_STYLE, WNDCLASSW, WS_OVERLAPPEDWINDOW,
        };

        let instance = match GetModuleHandleW(None) {
            Ok(handle) => handle,
            Err(error) => {
                tracing::error!(error = %error, "cannot get module handle for the tray window");
                return;
            }
        };
        let class_name = w!("SimpleHarTrayWnd");
        // ABI shim: the class wants an `extern "system"` proc; `DefWindowProcW` is a
        // plain Rust fn in this crate version.
        unsafe extern "system" fn wnd_proc(
            hwnd: windows::Win32::Foundation::HWND,
            msg: u32,
            wparam: windows::Win32::Foundation::WPARAM,
            lparam: windows::Win32::Foundation::LPARAM,
        ) -> windows::Win32::Foundation::LRESULT {
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        let window_class = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: instance.into(),
            lpszClassName: class_name,
            ..Default::default()
        };
        if RegisterClassW(&window_class) == 0 {
            tracing::warn!("tray window class registration failed; continuing");
        }
        // A hidden window so the tray has a message queue on this thread; never shown.
        let _hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            w!("SimpleHar tray"),
            WS_OVERLAPPEDWINDOW,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        );
        let mut last_state: Option<TrayState> = None;
        let mut last_tooltip: Option<String> = None;
        let mut message = MSG::default();
        loop {
            // Wake on any message or the refresh tick, whichever comes first.
            MsgWaitForMultipleObjectsEx(None, REFRESH_MS, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
            refresh(tray, &mut last_state, &mut last_tooltip);
        }
    }
    #[cfg(not(windows))]
    {
        // No message pump off Windows: tick on a plain sleep loop.
        let mut last_state: Option<TrayState> = None;
        let mut last_tooltip: Option<String> = None;
        loop {
            std::thread::sleep(std::time::Duration::from_millis(REFRESH_MS as u64));
            refresh(tray, &mut last_state, &mut last_tooltip);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::{JobStatus, SlotStatus, SwapState};

    fn status(slots: Vec<SlotStatus>, jobs: Vec<JobStatus>) -> SchedulerStatus {
        SchedulerStatus {
            slots,
            jobs,
            catalog_errors: Vec::new(),
        }
    }

    fn slot(category: CategoryId, model: Option<&str>) -> SlotStatus {
        SlotStatus {
            category,
            index: 0,
            model: model.map(str::to_string),
            port: model.map(|_| 12345),
            pid: model.map(|_| 1),
            need_gib: model.map(|_| 4.0),
            idle_s: model.map(|_| 0),
            serve_id: None,
            loading_job: None,
            loaded_at: std::time::Instant::now(),
        }
    }

    fn job(terminal: bool, error: Option<&str>) -> JobStatus {
        JobStatus {
            id: crate::scheduler::JobId(0),
            kind: crate::scheduler::JobKind::Load,
            model: "m".to_string(),
            category: CategoryId::Work,
            state: if terminal {
                SwapState::Serving
            } else {
                SwapState::Spawn
            },
            terminal,
            outcome: Some("loaded".to_string()),
            error: error.map(str::to_string),
            // Independently restated (this fixture is Load with a non-cancel outcome).
            is_fault: terminal && error.is_some(),
            need_gib: Some(4.0),
            free_at_admit_gib: Some(66.0),
            observed_at_admit_gib: Some(64.0),
            gate_target_gib: Some(4.0),
            victims_released: Vec::new(),
            warnings: Vec::new(),
            timings: Default::default(),
        }
    }

    #[test]
    fn colors_follow_the_operator_spec() {
        // Nothing loaded, no jobs, all listeners up: gray.
        assert_eq!(
            tray_state(&status(vec![slot(CategoryId::Work, None)], vec![]), false),
            TrayState::Gray
        );
        // Model loaded: green.
        assert_eq!(
            tray_state(
                &status(vec![slot(CategoryId::Work, Some("qwen3"))], vec![]),
                false
            ),
            TrayState::Green
        );
        // Load in flight: yellow, even with another model serving.
        assert_eq!(
            tray_state(
                &status(
                    vec![slot(CategoryId::Work, Some("qwen3"))],
                    vec![job(false, None)]
                ),
                false
            ),
            TrayState::Yellow
        );
        // A listener down is red, outranking yellow and green — harness-level
        // degradation while the app keeps running (operator, 2026-10-25).
        assert_eq!(
            tray_state(
                &status(
                    vec![slot(CategoryId::Work, Some("qwen3"))],
                    vec![job(false, None)]
                ),
                true
            ),
            TrayState::Red
        );
        // A failed load is a per-job error, NOT red: nothing loaded, listener up.
        assert_eq!(
            tray_state(
                &status(
                    vec![slot(CategoryId::Work, None)],
                    vec![job(true, Some("boom"))]
                ),
                false
            ),
            TrayState::Gray
        );
    }

    #[test]
    fn tooltip_lists_every_category_and_appends_errors() {
        let loaded_status = SchedulerStatus {
            slots: vec![
                slot(CategoryId::Work, Some("qwen3")),
                slot(CategoryId::Aux, Some("a")),
                slot2(CategoryId::Aux, Some("b")),
            ],
            jobs: vec![],
            catalog_errors: Vec::new(),
        };
        let text = tooltip(&loaded_status);
        assert!(text.contains("WORK: qwen3"), "was: {text}");
        assert!(text.contains("AUX: a, b"), "was: {text}");
        assert!(text.contains("RP: —"), "was: {text}");

        let errored = status(
            vec![slot(CategoryId::Work, None)],
            vec![job(true, Some("need 50 GiB"))],
        );
        assert!(tooltip(&errored).contains("need 50 GiB"));
    }

    fn slot2(category: CategoryId, model: Option<&str>) -> SlotStatus {
        SlotStatus {
            index: 1,
            ..slot(category, model)
        }
    }
}
