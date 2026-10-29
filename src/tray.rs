//! Tray icon (DESIGN Step 15, operator spec 2026-10-25; tri-state ruling 2026-10-28).
//!
//! The tray is a *view*: it renders the scheduler's status snapshot and the listener
//! registry, and sends the same scheduler commands the HTTP handlers send — no
//! tray-only state. Colors, in precedence order: **red** = a listener is down (the
//! reason rides in the tooltip), **amber** = no verdict — a category still `Pending`
//! after the boot grace, or a status fetch that returned nothing, yellow = a
//! load/unload in flight, green = a model is loaded, gray = nothing loaded. Per-job
//! errors are *not* red; they ride in the tooltip. This is the LISTENER palette and
//! stays untouched by config-state marking: the page's `stale_config` amber (Axis-2
//! ruling 2026-10-29, the row dot) is the CONFIG palette's and never drives the icon. Left click does nothing; the
//! right-click menu is Open Web UI / Unload All Models / Unload WORK / Unload RP /
//! Unload All AUX / Unload EMB / Exit.
//!
//! Nothing on this thread may wait on the actor without a bound: the pump thread owns
//! the hidden window, and the menu is shown by a modal `TrackPopupMenu` loop running
//! on it, so an unbounded `block_on` freezes the icon, the menu and the Exit item
//! together (FINDINGS-15 [3]). Every wait goes through [`bounded`], and Exit touches
//! no runtime at all ([`signal_shutdown`]).
//!
//! The Win32 message pump is the module's documented `unsafe`: one dedicated thread
//! owns a hidden window and pumps messages for the tray-icon crate; the safety
//! invariant is that all tray/menu interactions happen on, or are dispatched by, that
//! single thread (Step 8 convention: contained, justified, documented).

use crate::config::CategoryId;
use crate::scheduler::{SchedulerError, SchedulerHandle, SchedulerStatus};
use std::future::Future;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
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
/// How long one status fetch may hold the pump (FINDINGS-15 [3]): same number as the
/// cadence on purpose, so a wedged actor costs at most one tick instead of the icon.
const REFRESH_BUDGET: Duration = Duration::from_millis(2_000);
/// How long one menu command may hold the modal menu loop (FINDINGS-15 [3]): long
/// enough for a real unload to answer, short enough that a stalled actor cannot hold
/// the operator's menu — and never the Exit item, which is not a scheduler command.
const MENU_BUDGET: Duration = Duration::from_millis(5_000);
/// Boot grace before `Pending` turns amber (operator ruling "A + amber",
/// 2026-10-28).
///
/// The epoch is the registry's creation instant
/// ([`crate::listeners::ListenerRegistry::created_at`]) — `main` builds the registry
/// before the tray spawns and before any supervisor is spawned, so it is the earliest
/// honest "the harness started trying to listen" timestamp available to both sides.
/// Until the grace has elapsed the icon stays **gray**: the harness is booting, not
/// lying, and every verdict is milliseconds away. After it, a `Pending` category is a
/// supervisor that never spoke, which is exactly what amber means.
const BOOT_GRACE: Duration = Duration::from_millis(2_000);
/// Hover text for a tick with no scheduler verdict (FINDINGS-15 [1]). Kept inside the
/// 128 wchar `szTip` cap the tray-icon crate enforces.
const NO_VERDICT_TIP: &str =
    "SimpleHar — scheduler gave no verdict; listener states still decide the colour";

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

/// Tray icon state, derived from one scheduler snapshot plus the listener verdicts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayState {
    /// A model is loaded.
    Green,
    /// A load or unload is in flight.
    Yellow,
    /// Nothing loaded, and every category has a verdict.
    Gray,
    /// An error needs the operator's attention.
    Red,
    /// No verdict: a category is still `Pending` after [`BOOT_GRACE`], or this tick's
    /// status fetch returned nothing. The same word for both, per the 2026-10-28
    /// ruling.
    Amber,
}

/// Everything the colour decision needs beyond the scheduler snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TraySignal {
    /// Some category carries an explicit `Down` verdict — red outranks everything.
    pub any_down: bool,
    /// Some category is still `Pending` and [`BOOT_GRACE`] has elapsed — amber.
    pub pending_after_grace: bool,
    /// This tick could not read the scheduler at all. "No verdict" is the same
    /// vocabulary as `Pending`, so it is the same colour (FINDINGS-15 [1]).
    pub status_unreachable: bool,
}

/// Projects the listener registry onto the tray's listener facts, evaluated at `now`.
///
/// `now` is a parameter, not `Instant::now()` inside, so the grace boundary is
/// unit-testable without sleeping: pass `registry.created_at()` to stand inside the
/// boot window and `registry.created_at() + BOOT_GRACE` to stand after it.
///
/// # Arguments
///
/// * `listeners` - the shared registry.
/// * `status_unreachable` - whether this tick's scheduler read produced a verdict.
/// * `now` - the instant the tick is being evaluated at.
#[must_use]
pub fn tray_signal(
    listeners: &crate::listeners::ListenerRegistry,
    status_unreachable: bool,
    now: Instant,
) -> TraySignal {
    let waiting = !listeners.pending().is_empty();
    TraySignal {
        any_down: listeners.any_down(),
        pending_after_grace: waiting
            && now.saturating_duration_since(listeners.created_at()) >= BOOT_GRACE,
        status_unreachable,
    }
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
        // The header's "Left click does nothing" is a contract, not an observation
        // (FINDINGS2-15 [1]): tray-icon defaults `menu_on_left_click: true`
        // (tray-icon-0.21.3 src/lib.rs:209) and pops the modal menu on `WM_LBUTTONDOWN`
        // whenever a menu is attached (platform_impl/windows/mod.rs:462-467) — which
        // ours is, so a misclick opened the menu whose items act on the fleet. The
        // right-click menu is the only entry point the operator was told about.
        .with_menu_on_left_click(false)
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
/// Every operator-initiated action reports its own failure (FINDINGS-15 [7]) and
/// every wait on the actor is bounded (FINDINGS-15 [3]).
fn handle_menu(id: MenuId) {
    let Some(ctx) = CTX.get() else {
        return;
    };
    let Some(command) = command_for(id.as_ref()) else {
        return;
    };
    match command {
        Command::OpenWebUi => open_web_ui(ctx),
        Command::UnloadAll => {
            send_menu_command(
                &ctx.runtime,
                "Unload All Models",
                MENU_BUDGET,
                ctx.scheduler.unload_all(),
            );
        }
        Command::Unload(category) => send_menu_command(
            &ctx.runtime,
            &format!("Unload {category}"),
            MENU_BUDGET,
            ctx.scheduler.unload(category),
        ),
        Command::Exit => signal_shutdown(ctx),
    }
}

/// Opens the control page without a console window (defect E5). A spawn that never
/// happened used to be invisible (FINDINGS-15 [7]): a missing `cmd.exe` now lands in
/// the ring, where `/har/log` can answer "why did nothing open?".
///
/// The `CreateProcessW` behind `spawn()` runs on the **caller's** thread, and this
/// caller is the pump thread that owns the hidden window: a hung process-creation path
/// (AV minifilter, broken shell association, reparse driver) would freeze the icon, the
/// menu and the Exit item together — the same wedge FINDINGS-15 [3] put a budget on the
/// actor waits for, but no budget can interrupt a wedged syscall. So the spawn is handed
/// to a short-lived thread and the pump returns to pumping (FINDINGS2-15 [3]).
fn open_web_ui(ctx: &TrayCtx) {
    let url = ctx.control_url.clone();
    if let Err(error) = std::thread::Builder::new()
        .name("se-open-web-ui".to_string())
        .spawn(move || {
            let mut opener = std::process::Command::new("cmd");
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                opener.args(["/C", "start", "", &url]);
                opener.creation_flags(CREATE_NO_WINDOW);
            }
            #[cfg(not(windows))]
            opener.arg(&url);
            if let Err(error) = opener.spawn() {
                tracing::error!(error = %error, url = %url, "tray: cannot open the Web UI");
            }
        })
    {
        tracing::error!(
            error = %error,
            url = %ctx.control_url,
            "tray: cannot start the opener thread; the Web UI was not opened"
        );
    }
}

/// Sends one scheduler command from the menu: bounded, and never silent.
///
/// The two ways an operator's click can fail are the actor refusing (the scheduler's
/// own error) and the actor never answering (the bound expiring). Both used to be
/// dropped on the floor (FINDINGS-15 [7]), leaving the icon unchanged and the ring
/// with nothing to say about it.
fn send_menu_command<T>(
    runtime: &Handle,
    what: &str,
    budget: Duration,
    future: impl Future<Output = Result<T, SchedulerError>>,
) {
    match bounded(runtime, budget, future) {
        None => tracing::error!(
            tray_action = what,
            budget_ms = budget.as_millis() as u64,
            "tray: {what} timed out; the scheduler never answered"
        ),
        Some(Err(error)) => {
            tracing::error!(error = %error, tray_action = what, "tray: {what} failed");
        }
        Some(Ok(_)) => {}
    }
}

/// The Exit path: synchronous and unconditional (FINDINGS-15 [3]).
///
/// It touches the `watch::Sender` and nothing else — no runtime handle, no
/// `block_on`, no actor — so it cannot queue behind a wedged scheduler. The click
/// reaches here from the pump thread's `DispatchMessageW`; the bound on the refresh
/// tick is what guarantees the pump gets back to that dispatch within
/// [`REFRESH_BUDGET`], and `run()`'s `select!` then sees the flip immediately.
fn signal_shutdown(ctx: &TrayCtx) {
    if let Err(error) = ctx.shutdown.send(true) {
        tracing::error!(
            error = ?error,
            "tray: Exit could not signal shutdown (the receiver is already gone)"
        );
    }
}

/// Runs one future on the tray thread for at most `budget` (FINDINGS-15 [3]).
///
/// Every wait on the actor in this module goes through here. `None` means the bound
/// expired: the future was dropped (its request is abandoned, which is what a
/// never-answering actor deserves) and the caller keeps the pump alive; the caller
/// owns the error line, because only it knows which surface was at stake.
///
/// # Arguments
///
/// * `runtime` - the tokio handle `main`'s multi-thread runtime gave the tray thread.
/// * `budget` - the wall-clock bound.
/// * `future` - the call to run.
fn bounded<T>(runtime: &Handle, budget: Duration, future: impl Future<Output = T>) -> Option<T> {
    // The timeout is built INSIDE `block_on`: `tokio::time::timeout` registers a
    // timer with the reactor when it is created, and the tray thread is not inside
    // the runtime context until `block_on` enters it. Building it in the argument
    // position panics with "there is no reactor running" — which is precisely the
    // dead-icon failure mode this helper exists to prevent.
    runtime
        .block_on(async move { tokio::time::timeout(budget, future).await })
        .ok()
}

/// What the pump remembers between ticks: an unchanged icon is not repainted, and a
/// standing scheduler failure is reported once instead of every 2 s — a wedged actor
/// must not evict real evidence from the bounded log ring with a heartbeat.
#[derive(Default)]
struct TickMemo {
    state: Option<TrayState>,
    tooltip: Option<String>,
    fault: Option<String>,
}

impl TickMemo {
    /// Logs a standing fault the first time it is seen; a repeat of the same fault is
    /// silent, and the recovery line fires only when a verdict arrives again.
    fn record_fault(&mut self, fault: String) {
        if self.fault.as_deref() != Some(fault.as_str()) {
            tracing::error!(reason = %fault, "tray: no scheduler verdict; listener states still decide the icon");
        }
        self.fault = Some(fault);
    }

    /// Notes that a verdict arrived; says so once if a fault was standing.
    fn note_recovered(&mut self) {
        if self.fault.take().is_some() {
            tracing::info!("tray: scheduler verdicts are flowing again");
        }
    }
}

/// Refresh tick: derive the state, update icon and tooltip when they change. The
/// `TrayIcon` is not `Send`/`Sync` on Windows — it lives on, and is only touched by,
/// the tray thread (this function).
fn refresh(tray: &TrayIcon, memo: &mut TickMemo) {
    let Some(ctx) = CTX.get() else {
        return;
    };
    let outcome = bounded(&ctx.runtime, REFRESH_BUDGET, ctx.scheduler.status());
    let status = match outcome {
        Some(Ok(status)) => {
            memo.note_recovered();
            Some(status)
        }
        Some(Err(error)) => {
            memo.record_fault(format!("scheduler refused the status read: {error}"));
            None
        }
        None => {
            memo.record_fault(format!(
                "scheduler gave no status within {} ms",
                REFRESH_BUDGET.as_millis()
            ));
            None
        }
    };
    // FINDINGS-15 [1]: there is no early `return` on this path any more. A missing
    // snapshot still goes through the decision, so a `Down` listener paints red even
    // with the actor gone, and the icon is never frozen on the colour it last earned.
    let tick = tick(&ctx.listeners, status.as_ref(), Instant::now());
    if memo.state != Some(tick.state) {
        match solid_icon(tick.state) {
            Ok(icon) => {
                if let Err(error) = tray.set_icon(Some(icon)) {
                    tracing::error!(error = %error, state = ?tick.state, "tray: cannot set the icon");
                }
            }
            Err(error) => tracing::error!(error = %error, "tray icon render failed"),
        }
        memo.state = Some(tick.state);
    }
    if memo.tooltip.as_deref() != Some(tick.tooltip.as_str()) {
        if let Err(error) = tray.set_tooltip(Some(&tick.tooltip)) {
            tracing::error!(error = %error, "tray: cannot set the tooltip");
        }
        memo.tooltip = Some(tick.tooltip);
    }
}

/// What one tick decided: the colour to paint and the hover text to show.
struct Tick {
    state: TrayState,
    tooltip: String,
}

/// The whole tick decision, factored out of [`refresh`] so the no-freeze rule of
/// FINDINGS-15 [1] is unit-reachable without a live tray icon: `status` is `None`
/// when the fetch failed or timed out, and the registry is still consulted.
///
/// # Arguments
///
/// * `listeners` - the shared listener registry.
/// * `status` - this tick's scheduler snapshot, or `None` for "no verdict".
/// * `now` - the instant the tick is evaluated at (the boot-grace boundary).
fn tick(
    listeners: &crate::listeners::ListenerRegistry,
    status: Option<&SchedulerStatus>,
    now: Instant,
) -> Tick {
    let signal = tray_signal(listeners, status.is_none(), now);
    let reason = down_reason(listeners);
    let tooltip = match status {
        Some(snapshot) => tooltip(snapshot, reason.as_deref()),
        None => dead_tooltip(reason.as_deref()),
    };
    Tick {
        state: tray_state(status, &signal),
        tooltip,
    }
}

/// The tray state for one snapshot; precedence red > amber > yellow > green > gray
/// (Step 15; operator ruling "A + amber" 2026-10-28).
///
/// # Arguments
///
/// * `status` - the scheduler snapshot, or `None` when this tick read produced no
///   verdict. It is consulted only after red and amber are ruled out, so a `None`
///   can never freeze or soften the colour (FINDINGS-15 [1]).
/// * `signal` - the listener verdicts, plus whether the status read failed.
///
/// # Examples
///
/// ```
/// use simplehar_se::tray::{tray_state, TraySignal, TrayState};
/// use simplehar_se::scheduler::SchedulerStatus;
/// let status = SchedulerStatus {
///     slots: vec![],
///     jobs: vec![],
///     catalog_errors: vec![],
/// };
/// assert_eq!(
///     tray_state(Some(&status), &TraySignal::default()),
///     TrayState::Gray
/// );
/// assert_eq!(
///     tray_state(
///         None,
///         &TraySignal { any_down: true, ..TraySignal::default() }
///     ),
///     TrayState::Red,
///     "a dead listener outranks even a dead scheduler"
/// );
/// ```
#[must_use]
pub fn tray_state(status: Option<&SchedulerStatus>, signal: &TraySignal) -> TrayState {
    if signal.any_down {
        return TrayState::Red;
    }
    if signal.pending_after_grace || signal.status_unreachable {
        return TrayState::Amber;
    }
    let Some(status) = status else {
        // Unreachable by construction: a missing snapshot always arrives with
        // `status_unreachable`, which returned Amber above. Gray is the honest answer
        // to a caller that hands the decision neither a verdict nor a fault.
        return TrayState::Gray;
    };
    let in_flight = status.jobs.iter().any(|job| !job.terminal);
    let loaded = status.slots.iter().any(|slot| slot.model.is_some());
    if in_flight {
        TrayState::Yellow
    } else if loaded {
        TrayState::Green
    } else {
        TrayState::Gray
    }
}

/// The red state's reason, in the one sentence the operator needs (FINDINGS2-15 [2]).
///
/// `ListenerState::Down { reason }` carries the port and the bind error — the whole
/// story of the colour the icon is showing — and until now it lived only in the
/// registry and `/har/status`, while the header promised it in the tooltip. Categories
/// are walked in canonical slot-priority order so a harness with two dead listeners
/// names the one the operator would act on first.
fn down_reason(listeners: &crate::listeners::ListenerRegistry) -> Option<String> {
    let verdicts = listeners.snapshot();
    CategoryId::all()
        .iter()
        .filter_map(|category| {
            verdicts
                .get(category)
                .map(|state| (*category, state.clone()))
        })
        .find_map(|(category, state)| match state {
            crate::listeners::ListenerState::Down { reason } => {
                Some(format!("{category} listener is down: {reason}"))
            }
            _ => None,
        })
}

/// `NOTIFYICONDATAW.szTip` is a fixed `[u16; 128]` and tray-icon copies at most 128
/// UTF-16 units (tray-icon-0.21.3 src/platform_impl/windows/mod.rs:209-212). Anything
/// past that is cut silently, with no error from `set_tooltip` (FINDINGS-15 [4]).
const SOLTIP_CAPACITY: usize = 128;

/// Assembles hover text so the part that matters survives the truncation the shell
/// performs: `head` (prefix + the reason/fault lines) leads and `tail` (the category
/// list) is what gives way. The pre-fix layout appended the error last, which made it
/// the first casualty of the cap — exactly when the icon was red.
fn fit_soltip(head: &str, tail: &str) -> String {
    let head_units = head.encode_utf16().count();
    if head_units >= SOLTIP_CAPACITY {
        // A reason long enough to fill the tip on its own: the inventory is decoration,
        // the diagnosis is not.
        return elide_middle(head, SOLTIP_CAPACITY);
    }
    let budget = SOLTIP_CAPACITY - head_units;
    if tail.encode_utf16().count() <= budget {
        return format!("{head}{tail}");
    }
    format!("{}{}", head, elide_middle(tail, budget))
}

/// Truncates on a UTF-16 budget (the unit the shell counts in), on `char` boundaries,
/// keeping both ENDS: the front of a bind failure names the port and the tail names the
/// OS error, so a cut that keeps only one of them loses the diagnosis.
fn elide_middle(text: &str, max: usize) -> String {
    if text.encode_utf16().count() <= max {
        return text.to_string();
    }
    const MARK: &str = "...";
    let budget = max.saturating_sub(MARK.encode_utf16().count());
    let front_target = budget / 2;

    let mut front = String::new();
    let mut front_units = 0;
    let mut rest = text.chars();
    while let Some(ch) = rest.clone().next() {
        let need = ch.len_utf16();
        if front_units + need > front_target {
            break;
        }
        front.push(ch);
        front_units += need;
        rest.next();
    }

    let limit = budget - front_units;
    let mut back = String::new();
    let mut back_units = 0;
    for ch in rest.rev() {
        let need = ch.len_utf16();
        if back_units + need > limit {
            break;
        }
        back.insert(0, ch);
        back_units += need;
    }
    format!("{front}{MARK}{back}")
}

/// One line with every category's occupants, in canonical order.
fn category_line(status: &SchedulerStatus) -> String {
    let mut line = String::new();
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
    line
}

/// Hover text: the listener reason and the headline job fault first, then every
/// category's occupants, all inside what the shell will actually display.
///
/// # Arguments
///
/// * `status` - this tick's scheduler snapshot.
/// * `down_reason` - [`down_reason`]'s sentence when a listener is `Down`, `None`
///   otherwise. It leads the text because red is the colour that needs explaining.
#[must_use]
pub fn tooltip(status: &SchedulerStatus, down_reason: Option<&str>) -> String {
    let mut head = String::from("SimpleHar — ");
    if let Some(reason) = down_reason {
        head.push_str(&format!("{reason}\n"));
    }
    if let Some(fault) = crate::scheduler::headline_fault(&status.jobs) {
        if let Some(error) = &fault.error {
            head.push_str(&format!("{error}\n"));
        }
    }
    if head.ends_with('\n') {
        head.push_str("  ");
    }
    fit_soltip(&head, &category_line(status))
}

/// Hover text for a tick whose status fetch produced nothing (FINDINGS-15 [1]). The
/// colour is still decided by the registry, so the reason must be too: a red icon with
/// an actor gone still has to say which listener died.
fn dead_tooltip(down_reason: Option<&str>) -> String {
    match down_reason {
        Some(reason) => fit_soltip(&format!("{NO_VERDICT_TIP}\n"), reason),
        None => NO_VERDICT_TIP.to_string(),
    }
}

/// Renders a filled-circle icon of `state`'s color; no asset files (Step 15).
fn solid_icon(state: TrayState) -> Result<Icon, String> {
    let (red, green, blue): (u8, u8, u8) = match state {
        TrayState::Green => (0x2e, 0xa0, 0x43),
        TrayState::Yellow => (0xd8, 0x9b, 0x1d),
        // Amber is orange-shifted against the yellow of an in-flight swap so the two
        // are told apart at 32 px: "in flight" is the darker gold, "no verdict" the
        // brighter amber.
        TrayState::Amber => (0xf5, 0x8b, 0x00),
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
        let mut memo = TickMemo::default();
        let mut message = MSG::default();
        loop {
            // Wake on any message or the refresh tick, whichever comes first. A click
            // is dispatched before the (bounded) tick runs, and the tick can hold the
            // thread for at most `REFRESH_BUDGET`, so a queued Exit is never captive
            // to a wedged actor for longer than that.
            MsgWaitForMultipleObjectsEx(None, REFRESH_MS, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
            refresh(tray, &mut memo);
        }
    }
    #[cfg(not(windows))]
    {
        // No message pump off Windows: tick on a plain sleep loop.
        let mut memo = TickMemo::default();
        loop {
            std::thread::sleep(std::time::Duration::from_millis(REFRESH_MS as u64));
            refresh(tray, &mut memo);
        }
    }
}

/// Test-only capture subscriber: lets a test assert that a line was really logged.
/// Production logs reach the ring through `observability`'s layer, which is installed
/// only by `main`, so without this the pin "the failure is reported" would be
/// unfalsifiable (`cargo test` runs with no subscriber at all).
#[cfg(test)]
pub(crate) mod testlog {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use tracing::Subscriber;
    use tracing_subscriber::layer::{Context, SubscriberExt};
    use tracing_subscriber::Layer;

    #[derive(Clone)]
    struct Capture(Arc<Mutex<Vec<String>>>);

    impl Capture {
        fn push(&self, line: String) {
            self.0.lock().expect("capture lock").push(line);
        }

        fn lines(&self) -> Vec<String> {
            self.0.lock().expect("capture lock").clone()
        }
    }

    struct Recorder<'a> {
        message: &'a mut String,
        fields: &'a mut BTreeMap<String, String>,
    }

    impl<'a> tracing::field::Visit for Recorder<'a> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                *self.message = format!("{value:?}");
            } else {
                self.fields
                    .insert(field.name().to_string(), format!("{value:?}"));
            }
        }
    }

    impl<S> Layer<S> for Capture
    where
        S: Subscriber,
    {
        fn on_event(&self, event: &tracing::Event<'_>, _context: Context<'_, S>) {
            let mut message = String::new();
            let mut fields = BTreeMap::new();
            event.record(&mut Recorder {
                message: &mut message,
                fields: &mut fields,
            });
            let mut line = format!(
                "{} {} {}",
                event.metadata().level(),
                event.metadata().target(),
                message
            );
            for (name, value) in fields {
                line.push_str(&format!(" {name}={value}"));
            }
            self.push(line);
        }
    }

    /// A thread-local capture session; `stop` returns every line recorded so far.
    pub(crate) struct Session(Option<tracing::subscriber::DefaultGuard>, Capture);

    impl Session {
        /// Installs the capture subscriber on the calling thread. Use this (not
        /// [`with_capture`]) when the code under test must `await`: a current-thread
        /// test runtime keeps polling the future on this same thread.
        pub(crate) fn start() -> Self {
            let capture = Capture(Arc::new(Mutex::new(Vec::new())));
            let subscriber =
                tracing_subscriber::registry::Registry::default().with(capture.clone());
            Session(Some(tracing::subscriber::set_default(subscriber)), capture)
        }

        /// Uninstalls the subscriber and returns the recorded lines.
        pub(crate) fn stop(mut self) -> Vec<String> {
            drop(self.0.take());
            self.1.lines()
        }
    }

    /// Runs `run` with a subscriber that records every event it sees and returns the
    /// recorded lines alongside `run`'s value.
    pub(crate) fn with_capture<R>(run: impl FnOnce() -> R) -> (R, Vec<String>) {
        let session = Session::start();
        let result = run();
        (result, session.stop())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::listeners::ListenerRegistry;
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
            launched_at: None,
            stale_config: None,
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

    /// A registry with a verdict for every category except `unresolved`.
    fn registry_with(unresolved: Option<CategoryId>, down: Option<CategoryId>) -> ListenerRegistry {
        let registry = ListenerRegistry::new();
        for category in CategoryId::all() {
            if Some(category) == unresolved || Some(category) == down {
                continue;
            }
            registry.mark_serving(category);
        }
        if let Some(category) = down {
            registry.mark_down(category, format!("cannot bind {category}"));
        }
        registry
    }

    #[test]
    fn colors_follow_the_operator_spec() {
        let empty_signal = TraySignal::default();
        // Nothing loaded, no jobs, all listeners Serving: gray.
        assert_eq!(
            tray_state(
                Some(&status(vec![slot(CategoryId::Work, None)], vec![])),
                &empty_signal
            ),
            TrayState::Gray
        );
        // Model loaded: green.
        assert_eq!(
            tray_state(
                Some(&status(vec![slot(CategoryId::Work, Some("qwen3"))], vec![])),
                &empty_signal
            ),
            TrayState::Green
        );
        // Load in flight: yellow, even with another model serving.
        assert_eq!(
            tray_state(
                Some(&status(
                    vec![slot(CategoryId::Work, Some("qwen3"))],
                    vec![job(false, None)]
                )),
                &empty_signal
            ),
            TrayState::Yellow
        );
        // A listener down is red, outranking yellow and green — harness-level
        // degradation while the app keeps running (operator, 2026-10-25).
        assert_eq!(
            tray_state(
                Some(&status(
                    vec![slot(CategoryId::Work, Some("qwen3"))],
                    vec![job(false, None)]
                )),
                &TraySignal {
                    any_down: true,
                    ..TraySignal::default()
                }
            ),
            TrayState::Red
        );
        // A failed load is a per-job error, NOT red: nothing loaded, listener up.
        assert_eq!(
            tray_state(
                Some(&status(
                    vec![slot(CategoryId::Work, None)],
                    vec![job(true, Some("boom"))]
                )),
                &empty_signal
            ),
            TrayState::Gray
        );
    }

    #[test]
    fn precedence_is_red_over_amber_over_yellow_over_green_over_gray() {
        // The full matrix the "A + amber" ruling pins. Falsifier: swap any two rows
        // below (e.g. let amber outrank red, or yellow outrank amber) and this fails.
        let loaded_and_in_flight = status(
            vec![slot(CategoryId::Work, Some("qwen3"))],
            vec![job(false, None)],
        );
        let idle = status(vec![slot(CategoryId::Work, None)], vec![]);

        // red outranks everything, including amber and a wedged status read.
        assert_eq!(
            tray_state(
                None,
                &TraySignal {
                    any_down: true,
                    pending_after_grace: true,
                    status_unreachable: true,
                }
            ),
            TrayState::Red
        );
        assert_eq!(
            tray_state(
                Some(&loaded_and_in_flight),
                &TraySignal {
                    any_down: true,
                    ..TraySignal::default()
                }
            ),
            TrayState::Red
        );
        // amber outranks yellow (a load in flight) and green (a model serving): a
        // category with no verdict is the more urgent lie.
        assert_eq!(
            tray_state(
                Some(&loaded_and_in_flight),
                &TraySignal {
                    pending_after_grace: true,
                    ..TraySignal::default()
                }
            ),
            TrayState::Amber
        );
        // amber is what a status fetch with no verdict reads — "no verdict" is the one
        // word for both cases.
        assert_eq!(
            tray_state(
                Some(&loaded_and_in_flight),
                &TraySignal {
                    status_unreachable: true,
                    ..TraySignal::default()
                }
            ),
            TrayState::Amber
        );
        // then yellow, then green, then gray.
        assert_eq!(
            tray_state(
                Some(&status(
                    vec![slot(CategoryId::Work, Some("qwen3"))],
                    vec![job(false, None)]
                )),
                &TraySignal::default()
            ),
            TrayState::Yellow
        );
        assert_eq!(
            tray_state(
                Some(&status(vec![slot(CategoryId::Work, Some("qwen3"))], vec![])),
                &TraySignal::default()
            ),
            TrayState::Green
        );
        assert_eq!(
            tray_state(Some(&idle), &TraySignal::default()),
            TrayState::Gray,
            "every category has a verdict and nothing is loaded"
        );
    }

    #[test]
    fn amber_waits_for_the_boot_grace_then_holds() {
        // Pinned to the registry's own creation instant, so the boundary is exact and
        // no test sleeps. Falsifier: drop the grace comparison (amber on any
        // `Pending`) and the in-grace rows fail; flip it the other way (never amber)
        // and the after-grace row fails.
        let registry = ListenerRegistry::new();
        let began = registry.created_at();

        let at_boot = tray_signal(&registry, false, began);
        assert!(
            !at_boot.pending_after_grace,
            "inside the grace the harness is booting, not lying"
        );
        assert_eq!(
            tray_state(None, &at_boot),
            TrayState::Gray,
            "the gray-boot window is a pin, not an accident"
        );

        let just_before = tray_signal(
            &registry,
            false,
            began + BOOT_GRACE - Duration::from_millis(1),
        );
        assert!(
            !just_before.pending_after_grace,
            "one millisecond short of the grace is still booting"
        );
        assert_eq!(tray_state(None, &just_before), TrayState::Gray);

        let at_boundary = tray_signal(&registry, false, began + BOOT_GRACE);
        assert!(
            at_boundary.pending_after_grace,
            "the grace is inclusive of its own endpoint: {BOOT_GRACE:?}"
        );
        assert_eq!(tray_state(None, &at_boundary), TrayState::Amber);

        let long_after = tray_signal(&registry, false, began + BOOT_GRACE * 3);
        assert_eq!(tray_state(None, &long_after), TrayState::Amber);

        // A resolved registry never reads amber, and a Down still reads red inside the
        // grace: the grace excuses a missing verdict, never a known outage.
        let all_up = registry_with(None, None);
        assert_eq!(
            tray_signal(&all_up, false, began),
            TraySignal::default(),
            "four verdicts is a verdict for all of them"
        );
        let down_at_boot = registry_with(None, Some(CategoryId::Work));
        assert_eq!(
            tray_state(None, &tray_signal(&down_at_boot, false, began)),
            TrayState::Red,
            "a listener known dead is red on the first tick"
        );
    }

    #[test]
    fn a_status_failure_never_freezes_the_colour_and_never_suppresses_red() {
        // FINDINGS-15 [1]: with the scheduler unreachable the registry is STILL
        // evaluated. Falsifier: restore the early `return` on a failed status fetch
        // (the `None` arm bails before the decision) and the red row below fails.
        let registry = registry_with(None, Some(CategoryId::Aux));
        let after_grace = registry.created_at() + BOOT_GRACE * 2;

        let dead = tick(&registry, None, after_grace);
        assert_eq!(
            dead.state,
            TrayState::Red,
            "a `Down` must surface even when /har/status is unreachable"
        );
        // Pre-fix this asserted exact equality with `NO_VERDICT_TIP`, which pinned the
        // FINDINGS2-15 [2] defect: a red icon whose hover said nothing about why. The
        // pin's point (the no-verdict sentence survives an actor that is gone) is kept
        // as a prefix leg, and the reason the colour is red must now be in the text.
        assert!(
            dead.tooltip.starts_with(NO_VERDICT_TIP),
            "the honest no-verdict sentence is still there: {}",
            dead.tooltip
        );
        assert!(
            dead.tooltip
                .contains("AUX listener is down: cannot bind AUX"),
            "the reason that painted the icon red has to reach the hover: {}",
            dead.tooltip
        );

        let unreachable_all_up = tick(&registry_with(None, None), None, after_grace);
        assert_eq!(
            unreachable_all_up.state,
            TrayState::Amber,
            "no verdict reads amber, never the last green/yellow/gray"
        );

        // And a `Pending` category with no scheduler is amber, not a frozen gray.
        let pending = tick(&ListenerRegistry::new(), None, after_grace);
        assert_eq!(pending.state, TrayState::Amber);

        // A healthy tick still reports normally, so the failure path is the only
        // thing that changed.
        let healthy = tick(
            &registry_with(None, None),
            Some(&status(vec![slot(CategoryId::Work, Some("qwen3"))], vec![])),
            after_grace,
        );
        assert_eq!(healthy.state, TrayState::Green);
        assert!(
            healthy.tooltip.contains("WORK: qwen3"),
            "was: {}",
            healthy.tooltip
        );
    }

    #[test]
    fn tooltip_lists_every_category_and_leads_with_the_errors() {
        let loaded_status = SchedulerStatus {
            slots: vec![
                slot(CategoryId::Work, Some("qwen3")),
                slot(CategoryId::Aux, Some("a")),
                slot2(CategoryId::Aux, Some("b")),
            ],
            jobs: vec![],
            catalog_errors: Vec::new(),
        };
        let text = tooltip(&loaded_status, None);
        assert!(text.contains("WORK: qwen3"), "was: {text}");
        assert!(text.contains("AUX: a, b"), "was: {text}");
        assert!(text.contains("RP: —"), "was: {text}");
        assert!(
            text.starts_with("SimpleHar — WORK:"),
            "with nothing to explain the hover stays the one-line category list it \
             always was: {text}"
        );

        let errored = status(
            vec![slot(CategoryId::Work, None)],
            vec![job(true, Some("need 50 GiB"))],
        );
        let text = tooltip(&errored, None);
        assert!(text.contains("need 50 GiB"), "was: {text}");
        // FINDINGS-15 [4]: the error used to be APPENDED after the four-category list,
        // which made it the first casualty of the shell's 128-unit `szTip` cap.
        assert!(
            text.find("need 50 GiB") < text.find("WORK:"),
            "the explanation comes before the inventory: {text}"
        );
        assert!(
            NO_VERDICT_TIP.chars().count() < 128,
            "the tray-icon crate truncates szTip at 128 wchar: {NO_VERDICT_TIP}"
        );
    }

    /// FINDINGS2-15 [2] + FINDINGS-15 [4]: the listener `Down` reason never reached the
    /// hover at all, and even the job error that did was appended last — where the
    /// vendored `szTip` copy (`tray-icon-0.21.3 src/platform_impl/windows/mod.rs:209-212`,
    /// `for i in 0..tip.len().min(128)`) silently cuts it. Long model names are the
    /// realistic case that eats the budget. Falsifier: move the reason back behind the
    /// category list (append it instead of leading with it) and the tail assertion fails
    /// while the icon is red — the exact state that needs the sentence.
    #[test]
    fn the_red_reason_survives_the_soltip_cap() {
        let registry = ListenerRegistry::new();
        for category in CategoryId::all() {
            if category != CategoryId::Aux {
                registry.mark_serving(category);
            }
        }
        let reason = "cannot bind AUX listener on 127.0.0.1:8710: Only one usage of each \
                      socket address (protocol/network address/port) is normally permitted \
                      (os error 10048)";
        registry.mark_down(CategoryId::Aux, reason.to_string());
        let crowded = status(
            vec![
                slot(CategoryId::Work, Some("qwen2.5-coder-32b-instruct-q6_k")),
                slot(CategoryId::Rp, Some("artemis-31b-v1.2-q4_k-xl-merged")),
                slot(CategoryId::Aux, Some("ninfer-swift-nvfp4-uncensored")),
                slot(CategoryId::Emb, Some("embeddinggemma-2-GGUF-f16")),
            ],
            vec![],
        );

        let ticked = tick(&registry, Some(&crowded), Instant::now());
        assert_eq!(ticked.state, TrayState::Red);
        let units = ticked.tooltip.encode_utf16().count();
        assert!(
            units <= SOLTIP_CAPACITY,
            "the hover must be what the shell will actually display, not longer: \
             {units} units: {}",
            ticked.tooltip
        );
        assert!(
            ticked.tooltip.contains("os error 10048"),
            "the tail of the reason is the useful half (it names the port and the OS \
             error); appended-last lost it: {}",
            ticked.tooltip
        );
        assert!(
            ticked.tooltip.contains("AUX listener is down"),
            "{}",
            ticked.tooltip
        );

        // Same rule with the actor gone: red still has to say which listener died.
        let dead = tick(&registry, None, Instant::now());
        assert_eq!(dead.state, TrayState::Red);
        assert!(
            dead.tooltip.contains("os error 10048"),
            "a red icon with no status fetch still owes the reason: {}",
            dead.tooltip
        );
        assert!(
            dead.tooltip.encode_utf16().count() <= SOLTIP_CAPACITY,
            "{}",
            dead.tooltip
        );
    }

    /// FINDINGS2-15 [1]: the header tells the operator "Left click does nothing", and
    /// tray-icon's default is the opposite (`menu_on_left_click: true`, lib.rs:209) —
    /// with a menu attached, `WM_LBUTTONDOWN` pops the modal menu whose items unload
    /// models. The contract is enforced at the builder, so this is an inspection pin:
    /// the call may not quietly disappear in a refactor. Falsifier: drop
    /// `.with_menu_on_left_click(false)` and the assertion fails.
    #[test]
    fn left_click_stays_inert_because_the_builder_says_so() {
        let source = include_str!("tray.rs");
        let builder = source
            .split("TrayIconBuilder::new()")
            .nth(1)
            .expect("the tray is built from TrayIconBuilder")
            .split(".build()")
            .next()
            .expect("the builder chain terminates");
        assert!(
            builder.contains(".with_menu("),
            "the right-click menu is the documented entry point"
        );
        assert!(
            builder.contains(".with_menu_on_left_click(false)"),
            "without this call tray-icon opens the acting menu on a left click, \
             contradicting the module header"
        );
    }

    /// FINDINGS2-15 [3]: `CreateProcessW` runs on the caller's thread, and the menu
    /// dispatch IS the pump thread — so the open-the-page spawn has to leave it. A hung
    /// shell path (AV minifilter, broken association) would freeze icon + menu + Exit
    /// together, the same wedge FINDINGS-15 [3] budgets the actor waits against, except
    /// no budget can interrupt a wedged syscall. Inspection pin: the process spawn may
    /// not come back inline. Falsifier: run `opener.spawn()` directly in `open_web_ui`
    /// and the last assertion fails.
    #[test]
    fn opening_the_web_ui_never_runs_the_process_spawn_on_the_pump_thread() {
        let source = include_str!("tray.rs");
        let body = source
            .split("fn open_web_ui(ctx: &TrayCtx) {")
            .nth(1)
            .expect("open_web_ui is still here")
            .split("\n}\n")
            .next()
            .expect("the function closes");
        assert!(
            body.contains(".spawn(move ||"),
            "the work is handed to a thread: {body}"
        );
        let before_the_thread = body.split(".spawn(move ||").next().unwrap_or(body);
        assert!(
            !before_the_thread.contains("opener.spawn()"),
            "the browser command may not be spawned inline on the thread that owns the \
             hidden window: {body}"
        );
        assert!(
            body.contains("tracing::error!"),
            "and it still reports a spawn that never happened (FINDINGS-15 [7]): {body}"
        );
    }

    fn slot2(category: CategoryId, model: Option<&str>) -> SlotStatus {
        SlotStatus {
            index: 1,
            ..slot(category, model)
        }
    }

    #[test]
    fn a_stalled_call_hits_the_bound_and_the_caller_survives() {
        // FINDINGS-15 [3]: the pump must come back from a future that never
        // completes, say so, and stay usable. Falsifier: remove the timeout in
        // `bounded` (or raise `MENU_BUDGET` past the park below) and this hangs.
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let (_gate, park) = tokio::sync::oneshot::channel::<()>();
        let ((), lines) = super::testlog::with_capture(|| {
            let stalled = async move {
                park.await.ok();
                Ok::<(), SchedulerError>(())
            };
            send_menu_command(
                runtime.handle(),
                "Unload All Models",
                Duration::from_millis(50),
                stalled,
            );
        });
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.contains("timed out") && line.contains("Unload All Models"))
                .count(),
            1,
            "the bound must report the action it gave up on: {lines:?}"
        );
        // The caller — and the pump behind it — still works on the next call.
        let prompt = bounded(runtime.handle(), Duration::from_millis(500), async { 7_u8 });
        assert_eq!(
            prompt,
            Some(7),
            "a bound that fired must not poison the thread"
        );
        drop(_gate);
    }

    #[test]
    fn a_refused_command_is_logged_not_dropped() {
        // FINDINGS-15 [7]: the four `let _ =` sites used to swallow everything. A
        // scheduler refusal (actor gone) must reach the ring with the action named.
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let handle = SchedulerHandle::test_dummy();
        let ((), lines) = super::testlog::with_capture(|| {
            send_menu_command(
                runtime.handle(),
                "Unload WORK",
                MENU_BUDGET,
                handle.unload(CategoryId::Work),
            );
        });
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.contains("Unload WORK") && line.contains("failed"))
                .count(),
            1,
            "the refusal must be an error line naming the action: {lines:?}"
        );
    }

    #[test]
    fn exit_signals_shutdown_while_an_actor_call_is_still_parked() {
        // FINDINGS-15 [3]: Exit must never queue behind the actor. The menu path for
        // Exit touches only the watch sender, so a call parked in another thread —
        // the exact shape of a wedged actor holding the pump — cannot delay it.
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let (shutdown, mut shutdown_rx) = watch::channel(false);
        let ctx = TrayCtx {
            scheduler: SchedulerHandle::test_dummy(),
            listeners: ListenerRegistry::new(),
            control_url: "http://127.0.0.1:8708/".to_string(),
            runtime: runtime.handle().clone(),
            shutdown,
        };
        let (_gate, park) = tokio::sync::oneshot::channel::<()>();
        let wedged = {
            let handle = runtime.handle().clone();
            std::thread::spawn(move || {
                bounded(&handle, Duration::from_millis(200), async move {
                    park.await.ok();
                })
            })
        };
        let began = Instant::now();
        signal_shutdown(&ctx);
        assert!(
            began.elapsed() < Duration::from_millis(100),
            "the shutdown signal is synchronous: it must not wait on the parked call"
        );
        assert!(
            *shutdown_rx.borrow_and_update(),
            "the operator's Exit click reaches run()"
        );
        assert_eq!(
            wedged.join().expect("join the wedged call"),
            None,
            "and the parked call is what the bound reaps, not the Exit path"
        );
    }
}
