use gtk4::prelude::*;
use gtk4::{self as gtk, gio, glib};
use libadwaita as adw;
use libadwaita::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use crate::config;
use crate::hardware::{rgb, sensors, setup};
use crate::tray::TrayManager;
use crate::ui::{
    ai_page, background, battery_page, dashboard_page, drivers_page, fan_control_page, fan_page,
    gpu_page, monitor_page, network_page, rgb_page, setup_page, temperatures_page, tools_page,
    usage_page,
};

thread_local! {
    static HOLD_GUARD: RefCell<Option<gio::ApplicationHoldGuard>> = const { RefCell::new(None) };
    static TRAY: RefCell<Option<TrayManager>> = const { RefCell::new(None) };
}

/// `GtkWindow:suspended`, GTK 4.12 and up: the compositor's own word for
/// "this window is not being shown", which covers minimized, fully obscured,
/// and on another workspace.
const SUSPENDED_PROPERTY: &str = "suspended";

pub fn build(app: &adw::Application) {
    crate::startup_mark("window build start");
    // libadwaita window. `AdwApplicationWindow` is a `GtkApplicationWindow`
    // subclass that understands `AdwToolbarView`/`AdwHeaderBar`, so titlebar,
    // CSD and window controls are the platform's job now. The hand-drawn
    // header this used to carry (brand mark, fake minimize/close buttons,
    // neon edge bars) is gone.
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Predator Sense")
        .default_width(1200)
        .default_height(820)
        .resizable(true)
        .build();
    window.connect_map(|_| crate::startup_mark("window mapped"));

    // Check module status
    let module_status = setup::check_status();
    crate::startup_mark("module status checked");
    if matches!(
        module_status,
        setup::ModuleStatus::Ready | setup::ModuleStatus::AlternativeDriver
    ) {
        build_main_ui(app, &window);
    } else {
        build_with_setup(app, &window);
    }
    crate::startup_mark("window content built");

    // The compositor knows when the window stops being looked at - minimized,
    // fully covered, on another workspace - and nothing else here does: a
    // minimized window stays mapped, and its widgets with it, so every guard
    // written as `is_mapped()` keeps saying yes to a window nobody can see.
    //
    // Reached through the property rather than `is_suspended()`, which needs
    // the crate's `v4_12` feature: raising the compile-time API level surfaces
    // a dozen unrelated deprecations across this crate, and this way a runtime
    // GTK older than 4.12 just keeps the previous behaviour instead of
    // panicking on a property that is not there.
    if window.find_property(SUSPENDED_PROPERTY).is_some() {
        window.connect_notify_local(Some(SUSPENDED_PROPERTY), |window, _| {
            crate::app_state::set_window_suspended(window.property::<bool>(SUSPENDED_PROPERTY));
        });
    }
    sync_window_suspended(&window);

    // Live app-wide recolor by active power profile (Quiet/Balanced/
    // Performance/Turbo, each its own color - see `brand_theme.rs`):
    // applied once immediately, so a mode already active before this
    // launch is reflected right away and not just on the next change, then
    // reconciled every couple seconds after - same poll-and-reconcile shape
    // `fan_page.rs`'s own card highlighting already uses, just for the
    // whole app's theme instead of one page's cards. A dedicated timer here
    // rather than piggybacked on `fan_page.rs`'s own: that one only exists
    // once Mode has been opened at least once, and a profile change from
    // anywhere else (the AI assistant, GameSync, the physical mode key)
    // has to recolor the app even if the user never opens that page.
    {
        let window_c = window.clone();
        // Tracks the real active profile *and* the "keep default color"
        // setting together: either one changing has to re-apply the theme
        // right away, not just a profile switch - flipping the setting in
        // Settings with the mode unchanged must revert (or restore) the
        // color immediately, not silently wait for the next mode change to
        // take effect.
        type ThemeState = (Option<crate::hardware::profile::PowerProfile>, bool);
        let last_state: Rc<Cell<ThemeState>> = Rc::new(Cell::new((None, false)));
        let apply_if_changed = move || {
            let now = crate::hardware::profile::get_current_profile();
            let keep_default = config::load_app_config().keep_default_theme_color;
            let state = (now, keep_default);
            if state != last_state.get() {
                last_state.set(state);
                crate::apply_active_profile_theme(if keep_default { None } else { now });
                // The CSS reload above already recolors every `@cyan`-using
                // widget on its own (GTK re-styles reactively); this is only
                // for the hand-drawn Cairo chrome (sidebar highlight, the
                // Mode cards' own accent, gauges) that reads
                // `brand_theme::accent()` directly and needs an explicit
                // nudge to actually repaint with the new value.
                queue_draw_recursive(window_c.upcast_ref());
            }
        };
        apply_if_changed();
        glib::timeout_add_seconds_local(2, move || {
            apply_if_changed();
            glib::ControlFlow::Continue
        });
    }

    // Handle ALL close events (native X button, our custom button, Alt+F4, etc.)
    let app_clone = app.clone();
    window.connect_close_request(move |win| {
        let cfg = config::load_app_config();
        eprintln!("[close] minimize_on_close={}", cfg.minimize_on_close);
        if cfg.minimize_on_close {
            hide_to_tray(win, &app_clone);
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });

    // Opens maximized every time: fixed pixel dimensions (the sidebar and
    // several cards among them - see the width fixes nearby) were sized
    // against Portuguese/English text, and a language with longer words
    // (Russian, German) can need more room than a small window leaves.
    // Maximized gives every language the most space the screen has, rather
    // than papering over the same layouts at one fixed default size.
    // Opens maximized every time, exactly as before this rewrite: several
    // pages (Lighting's four zones, Mode's cards, Settings' rows) are wider
    // than a small window and only scroll vertically. On a tiling WM the
    // alternative is whatever tile the compositor hands out, which clips
    // them - maximized gives every language and every page the most space
    // the screen has.
    window.maximize();
    window.present();
    crate::startup_mark("window present called");
}

/// Reads the compositor's current answer into the shared flag.
///
/// Called again after presenting the window, because presenting is a request
/// and not a guarantee: on Wayland the compositor can decline to restore a
/// minimized window, and then the property never changes, so no notification
/// arrives to correct anything. Assuming the window came back would start every
/// animation up again behind a window that is still minimized, which is the
/// exact cost the guards exist to avoid.
pub fn sync_window_suspended(window: &impl IsA<gtk::Window>) {
    let window = window.as_ref();
    if window.find_property(SUSPENDED_PROPERTY).is_some() {
        crate::app_state::set_window_suspended(window.property::<bool>(SUSPENDED_PROPERTY));
    }
}

/// Esconde a janela e garante que o tray helper está rodando.
/// Unifica o comportamento do botão ✕ custom e do close request do WM.
fn hide_to_tray<W: IsA<gtk::Widget>>(win: &W, app: &adw::Application) {
    crate::app_state::set_window_visible(false);
    win.set_visible(false);
    // Mantém app viva enquanto estiver no tray
    HOLD_GUARD.with(|g| {
        if g.borrow().is_none() {
            *g.borrow_mut() = Some(app.hold());
        }
    });
    // Garante tray rodando (reinicia se morreu)
    TRAY.with(|t| {
        let mut tray = t.borrow_mut();
        let need_start = match tray.as_ref() {
            Some(tm) => !tm.started,
            None => true,
        };
        if need_start {
            let mut tm = TrayManager::new();
            tm.start();
            *tray = Some(tm);
        } else if let Some(tm) = tray.as_mut() {
            // Revalida: o processo pode ter morrido; start() é idempotente e re-spawna se preciso.
            tm.start();
        }
    });
    eprintln!("[close] janela escondida, tray iniciado");
}

fn build_with_setup(app: &adw::Application, window: &adw::ApplicationWindow) {
    // Once the wizard finishes, the window's whole content is replaced by the
    // real UI (the wizard used to be one page of a stack that build_main_ui
    // later appended to).
    let app_c = app.clone();
    let window_c = window.clone();
    let on_complete: Rc<dyn Fn()> = Rc::new(move || {
        build_main_ui(&app_c, &window_c);
    });
    let setup = setup_page::build(on_complete);

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&adw::HeaderBar::new());
    toolbar_view.set_content(Some(&setup));
    window.set_content(Some(&toolbar_view));
}

fn build_main_ui(app: &adw::Application, window: &adw::ApplicationWindow) {
    let main_content = build_main_content(app, window);

    // Background watcher: critical-temperature alerts + auto power profile.
    // Runs every 5s regardless of window visibility (works in the tray too).
    {
        let cfg = config::load_app_config();
        crate::hardware::alerts::set_enabled(cfg.temp_alerts);
        crate::hardware::power_profile::set_auto(cfg.auto_profile_ac);
        crate::hardware::power_profile::set_target_profiles(cfg.profile_ac, cfg.profile_battery);
        crate::hardware::applog::set_enabled(cfg.debug_logging);
        crate::hardware::profile::set_keep_fan_auto_in_performance(
            cfg.keep_fan_auto_in_performance,
        );
        crate::hardware::profile::set_manage_cpu_power(cfg.manage_cpu_power);
        crate::hardware::game_sync::set_enabled(cfg.game_sync_enabled);

        // Fan Control page (ui::fan_control_page): CoolBoost and fan mode
        // used to only ever be written straight to the EC with nothing
        // remembering the choice, so closing Predator Sense (or a reboot
        // resetting the EC) silently dropped them - reapply them here on
        // every start. Off the GTK thread since each helper write costs
        // roughly 150ms and can trigger a polkit prompt.
        let coolboost_enabled = cfg.coolboost_enabled;
        let fan_mode = cfg.fan_mode.clone();
        if crate::hardware::capabilities::get().ec && (coolboost_enabled || fan_mode.is_some()) {
            background::run(
                move || {
                    if coolboost_enabled {
                        let _ = crate::hardware::fan::set_coolboost(true);
                    }
                    match fan_mode.as_deref() {
                        Some("max") => {
                            let _ = crate::hardware::fan::set_fan_mode(
                                crate::hardware::fan::FanMode::Max,
                            );
                        }
                        Some("auto") => {
                            let _ = crate::hardware::fan::set_fan_mode(
                                crate::hardware::fan::FanMode::Auto,
                            );
                        }
                        _ => {}
                    }
                },
                |()| {},
            );
        }

        glib::timeout_add_seconds_local(5, || {
            let (cpu, gpu) = sensors::read_critical_temps();
            crate::hardware::alerts::check(cpu, gpu);
            crate::hardware::power_profile::check();
            // Re-reads the game list from config every tick (cheap: a small
            // Vec clone), same reasoning as re-reading `ai_check_interval_min`
            // below - editing the list in the UI takes effect on the next
            // tick, no restart needed.
            crate::hardware::game_sync::check(&config::load_app_config().game_profiles);
            glib::ControlFlow::Continue
        });

        // Software auto fan-curve (fan_control_page.rs "fan_auto_curve"
        // switch): runs globally rather than only while that page is open,
        // since pages here are built lazily on first navigation - a
        // page-local timer would never restart the curve after a fresh
        // launch until the user visited Fan Control again. Re-reads the
        // config every tick, same as the game list above: toggling the
        // switch takes effect on the next tick, no restart needed.
        if crate::hardware::capabilities::get().fan_pwm {
            // The write goes through the persistent privileged helper (see
            // hardware::helper), which holds a mutex shared with every other
            // privileged caller in the app - calling it straight from this
            // timer would block the main thread (and every other feature's
            // privileged call) for the length of a pkexec auth prompt or a
            // full session respawn, so it runs on a worker thread instead.
            // `applying` skips a tick instead of queuing a second write if
            // the previous one is still in flight.
            let applying = Rc::new(Cell::new(false));
            glib::timeout_add_seconds_local(3, move || {
                let cfg = config::load_app_config();
                if cfg.fan_auto_curve_enabled && !applying.get() {
                    // Whichever die is hotter. The GPU reading was dropped
                    // here, so a GPU-bound load with an idle CPU got no fan
                    // response at all. See fan::curve_input_temp for why this
                    // stays one speed for both fans.
                    let (cpu, gpu) = sensors::read_critical_temps();
                    if let Some(t) = crate::hardware::fan::curve_input_temp(cpu, gpu) {
                        let pct = crate::hardware::fan::fan_curve_pct(t, &cfg.fan_curve_points);
                        applying.set(true);
                        let applying_done = applying.clone();
                        background::run(
                            move || crate::hardware::fan::set_pwm_percent(pct, pct),
                            move |_| applying_done.set(false),
                        );
                    }
                }
                glib::ControlFlow::Continue
            });
        }
    }

    // AI assistant background monitor (opt-in, off unless ai_assistant_enabled).
    // Snapshots hardware state every minute; once `ai_check_interval_min`
    // minutes' worth of snapshots have accumulated, asks Ollama for a
    // verdict (same confirm-or-auto-apply gate as the manual/chat triggers
    // on the AI page). Re-reads the interval from config on every tick, so
    // changing it in Settings takes effect on the next tick - no restart.
    {
        let window_c = window.clone();
        let minutes_since_check: Rc<std::cell::Cell<u32>> = Rc::new(std::cell::Cell::new(0));
        glib::timeout_add_seconds_local(60, move || {
            let cfg = config::load_app_config();
            if !cfg.ai_assistant_enabled {
                minutes_since_check.set(0);
                return glib::ControlFlow::Continue;
            }
            crate::hardware::ai_snapshot::append_snapshot();
            let elapsed = minutes_since_check.get() + 1;
            if elapsed >= cfg.ai_check_interval_min.max(1) {
                minutes_since_check.set(0);
                ai_page::run_periodic_check(window_c.upcast_ref());
            } else {
                minutes_since_check.set(elapsed);
            }
            glib::ControlFlow::Continue
        });
    }

    // Physical Predator/Turbo keyboard key reaction (see profile.rs's
    // TURBO_BUTTON_SYSFS doc comment and kernel/facer.c's turbo_state sysfs
    // patch). The key itself only ever toggles fan mode/OC/LED at the WMI
    // level - it never touches cpufreq governor/EPP/min_perf, so on its own
    // it can never make the "Modo" page show Turbo. Polling this attribute
    // and calling our own set_profile()/set_fan_mode() on a transition is
    // what makes the key match "press it, everything becomes consistently
    // Turbo" - both pages then update on their own via the live-refresh
    // polling already in fan_page.rs/fan_control_page.rs, no direct
    // knowledge of this timer needed there.
    {
        use crate::hardware::profile;
        let last_state: Rc<std::cell::Cell<Option<bool>>> = Rc::new(std::cell::Cell::new(None));
        // What to go back to on release - whatever was actually active right
        // before the key forced Turbo, not a hardcoded guess. Fixes a real
        // bug: releasing the key used to always force Balanced, silently
        // discarding Quiet/Performance/anything else the user had picked
        // before pressing it (and, since Balanced's EPP differs from
        // Quiet's, could leave the machine warmer than the user actually
        // asked for with no visible sign anything overrode their choice).
        let before_turbo: Rc<std::cell::Cell<Option<profile::PowerProfile>>> =
            Rc::new(std::cell::Cell::new(None));
        glib::timeout_add_seconds_local(2, move || {
            let Some(now) = profile::get_turbo_button_state() else {
                return glib::ControlFlow::Continue; // no such attribute on this hardware/kernel module
            };
            if last_state.get() == Some(now) {
                return glib::ControlFlow::Continue;
            }
            let first_read = last_state.get().is_none();
            last_state.set(Some(now));
            if first_read {
                // Don't act on whatever the state happened to be at app
                // startup - only react to an actual transition from here on.
                return glib::ControlFlow::Continue;
            }
            // set_profile() now applies the matching fan mode itself (Max
            // for Turbo, Auto otherwise), so this only needs to pick the
            // profile - no separate fan::set_fan_mode call to keep in sync.
            if now {
                // Snapshot whichever profile is coherently active right now,
                // before overwriting it - None (no single coherent profile
                // to snapshot) is handled the same way GameSync already
                // handles it: the key still gets its Turbo, restoring on
                // release just falls back to Balanced since there is
                // nothing truthful to go back to.
                before_turbo.set(profile::coherent_profile());
                let _ = profile::set_profile(profile::PowerProfile::Turbo);
                crate::hardware::applog::info("Turbo key: pressed, forced profile=Turbo fan=Max");
            } else {
                let restore = before_turbo
                    .get()
                    .unwrap_or(profile::PowerProfile::Balanced);
                let _ = profile::set_profile(restore);
                crate::hardware::applog::info(&format!(
                    "Turbo key: released, restored profile={} fan={}",
                    restore.to_id(),
                    if restore == profile::PowerProfile::Turbo
                        || restore == profile::PowerProfile::Performance
                    {
                        "Max"
                    } else {
                        "Auto"
                    }
                ));
            }
            glib::ControlFlow::Continue
        });
    }

    window.set_content(Some(&main_content));
}

type PageBuilder = Box<dyn FnOnce() -> gtk::Widget>;
type PendingPages = Rc<RefCell<HashMap<String, PageBuilder>>>;

/// Builds a page on first navigation.
///
/// The builder lives in `pending` until it has run once, so "still in
/// `pending`" is the not-built-yet marker: no need to ask the stack, and it
/// stays correct when a page (see the temperatures timer) is replaced later.
fn ensure_page_built(stack: &adw::ViewStack, pending: &PendingPages, name: &str, title: &str) {
    // Remove first so the RefCell borrow is released before the builder runs;
    // page constructors can register callbacks that immediately touch GTK.
    let builder = pending.borrow_mut().remove(name);
    let Some(builder) = builder else {
        return;
    };
    let page = builder();
    stack.add_titled(&page, Some(name), title);
    crate::startup_mark(&format!("lazy page: {name}"));
}

/// Builds the main window body: an `AdwNavigationSplitView` whose sidebar is
/// a `.navigation-sidebar` `GtkListBox` (the list style GNOME's own apps use)
/// and whose content is an `AdwViewStack` with one page per entry.
///
/// Page builders, the lazy-build registry, the background watchers and the
/// temperatures refresh all keep working exactly as they did under the
/// previous hand-drawn shell - only the chrome around them changed.
fn build_main_content(
    app: &adw::Application,
    window: &adw::ApplicationWindow,
) -> adw::NavigationSplitView {
    let stack = adw::ViewStack::new();
    stack.set_hexpand(true);
    stack.set_vexpand(true);
    // Homogeneous sizing (the GTK default) makes the window's minimum size
    // track whichever page has ever been built, however heavy - once
    // Temperaturas is opened once, later app starts would inherit its larger
    // gauges before the user ever sees that page. Size to whatever page is
    // actually showing instead.
    stack.set_hhomogeneous(false);
    stack.set_vhomogeneous(false);

    let dashboard = dashboard_page::build();
    crate::startup_mark("dashboard page");
    stack.add_titled(&dashboard, Some("home"), &crate::i18n::t("home_page"));

    // GTK widgets must be created on the main thread, but invisible pages do
    // not need to exist before the first frame. Build each page once, on its
    // first navigation, so its widgets, timers and hardware reads cannot hold
    // up application startup.
    let pending: PendingPages = Rc::new(RefCell::new(HashMap::new()));
    {
        let mut pages = pending.borrow_mut();
        pages.insert(
            "temperatures".into(),
            Box::new(|| {
                let readings = sensors::read_all_sensors();
                temperatures_page::build(&readings).upcast()
            }),
        );
        pages.insert(
            "network".into(),
            Box::new(|| network_page::build().upcast()),
        );
        pages.insert("usage".into(), Box::new(|| usage_page::build().upcast()));
        pages.insert("lighting".into(), Box::new(|| rgb_page::build().upcast()));
        pages.insert("fan".into(), Box::new(|| fan_page::build().upcast()));
        pages.insert(
            "fan_ctrl".into(),
            Box::new(|| fan_control_page::build().upcast()),
        );
        pages.insert(
            "battery".into(),
            Box::new(|| battery_page::build().upcast()),
        );
        pages.insert("gpu".into(), Box::new(|| gpu_page::build().upcast()));
        pages.insert(
            "monitor".into(),
            Box::new(|| monitor_page::build().upcast()),
        );
        pages.insert(
            "drivers".into(),
            Box::new(|| drivers_page::build().upcast()),
        );
        // GameSync, Macros and the AI assistant live under one "Tools" hub
        // (`tools_page.rs`); each tool's build() call runs inside that page,
        // still lazy (only on the first visit to "tools" itself).
        let window_weak = window.downgrade();
        pages.insert(
            "tools".into(),
            Box::new(move || {
                let window = window_weak
                    .upgrade()
                    .expect("lazy Tools page is built only while its window exists");
                tools_page::build(window.upcast_ref()).upcast()
            }),
        );
        let app_weak = app.downgrade();
        pages.insert(
            "settings".into(),
            Box::new(move || {
                let app = app_weak
                    .upgrade()
                    .expect("lazy settings page is built only while its app exists");
                build_settings_page(&app).upcast()
            }),
        );
    }

    // Menu items
    let nav_items = vec![
        (crate::i18n::t("home_page"), "home"),
        (crate::i18n::t("temperatures"), "temperatures"),
        (crate::i18n::t("usage"), "usage"),
        (crate::i18n::t("network"), "network"),
        (crate::i18n::t("lighting"), "lighting"),
        (crate::i18n::t("perf_mode"), "fan"),
        (crate::i18n::t("fan_control"), "fan_ctrl"),
        (crate::i18n::t("battery"), "battery"),
        (crate::i18n::t("gpu_menu"), "gpu"),
        (crate::i18n::t("monitoring"), "monitor"),
        (crate::i18n::t("tools_nav"), "tools"),
        (crate::i18n::t("drivers_and_manuals"), "drivers"),
        (crate::i18n::t("settings"), "settings"),
    ];
    let nav: Rc<Vec<(String, String)>> = Rc::new(
        nav_items
            .iter()
            .map(|(label, name)| (label.to_string(), name.to_string()))
            .collect(),
    );

    // === SIDEBAR ===
    let list = gtk::ListBox::new();
    list.add_css_class("navigation-sidebar");
    list.set_selection_mode(gtk::SelectionMode::Single);
    for (label, _) in &nav_items {
        let row = gtk::ListBoxRow::new();
        let lbl = gtk::Label::new(Some(label));
        lbl.set_halign(gtk::Align::Start);
        lbl.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        lbl.set_margin_start(12);
        lbl.set_margin_end(12);
        lbl.set_margin_top(8);
        lbl.set_margin_bottom(8);
        row.set_child(Some(&lbl));
        list.append(&row);
    }

    // Row selection is the single navigation path: clicking a row selects it
    // (the list also handles Up/Down once it has focus), and programmatic
    // navigation selects the row too, so the highlight and the visible page
    // cannot drift apart.
    let navigate_to: Rc<dyn Fn(usize)> = {
        let stack_c = stack.clone();
        let pending_c = pending.clone();
        let list_c = list.clone();
        let nav_c = nav.clone();
        Rc::new(move |idx: usize| {
            let Some((label, name)) = nav_c.get(idx) else {
                return;
            };
            ensure_page_built(&stack_c, &pending_c, name, label);
            stack_c.set_visible_child_name(name);
            if let Some(row) = list_c.row_at_index(idx as i32) {
                if list_c.selected_row().as_ref() != Some(&row) {
                    list_c.select_row(Some(&row));
                }
            }
        })
    };
    {
        let navigate_c = navigate_to.clone();
        list.connect_row_selected(move |_, row| {
            if let Some(row) = row {
                navigate_c(row.index() as usize);
            }
        });
    }

    let sidebar_scroll = gtk::ScrolledWindow::new();
    sidebar_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    sidebar_scroll.set_vexpand(true);
    sidebar_scroll.set_child(Some(&list));

    // Model / version / module status, pinned under the list.
    let status_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    status_box.set_margin_top(6);
    status_box.set_margin_bottom(12);
    status_box.set_halign(gtk::Align::Center);
    let model_name = std::fs::read_to_string("/sys/class/dmi/id/product_name")
        .unwrap_or_else(|_| "Predator".into());
    let model = gtk::Label::new(Some(model_name.trim()));
    model.add_css_class("info-text");
    status_box.append(&model);
    let ver = gtk::Label::new(Some(&format!("v{} • Linux", env!("CARGO_PKG_VERSION"))));
    ver.add_css_class("info-text-dim");
    status_box.append(&ver);
    let status_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    status_row.set_halign(gtk::Align::Center);
    status_row.set_margin_top(4);
    let module_loaded = rgb::is_module_loaded();
    let dot = gtk::Label::new(Some("●"));
    dot.add_css_class(if module_loaded {
        "status-dot-on"
    } else {
        "status-dot-off"
    });
    let st = gtk::Label::new(Some(crate::i18n::t(if module_loaded {
        "module_active"
    } else {
        "module_inactive"
    })));
    st.add_css_class("info-text-dim");
    status_row.append(&dot);
    status_row.append(&st);
    status_box.append(&status_row);

    let sidebar_content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    sidebar_content.append(&sidebar_scroll);
    sidebar_content.append(&status_box);

    let sidebar_view = adw::ToolbarView::new();
    sidebar_view.add_top_bar(&adw::HeaderBar::new());
    sidebar_view.set_content(Some(&sidebar_content));
    let sidebar_page = adw::NavigationPage::new(&sidebar_view, "Predator Sense");

    // === CONTENT ===
    let title = adw::WindowTitle::new(&crate::i18n::t("home_page"), "");
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&title));
    {
        let title_c = title.clone();
        stack.connect_visible_child_notify(move |s| {
            let label = s
                .visible_child()
                .and_then(|child| s.page(&child).title())
                .map(|t| t.to_string())
                .unwrap_or_default();
            title_c.set_title(&label);
        });
    }

    let content_view = adw::ToolbarView::new();
    content_view.add_top_bar(&header);
    content_view.set_content(Some(&stack));
    let content_page = adw::NavigationPage::new(&content_view, "Predator Sense");

    let split = adw::NavigationSplitView::new();
    split.set_sidebar(Some(&sidebar_page));
    split.set_content(Some(&content_page));

    // Land on the first page, with its row highlighted.
    navigate_to(0);

    // Refresh periódico da página de temperaturas (gauges precisam recalcular).
    // Gated por visibilidade do window e da aba — evita rebuild quando no tray.
    {
        let stack_c = stack.clone();
        glib::timeout_add_seconds_local(2, move || {
            if !crate::app_state::is_window_visible() {
                return glib::ControlFlow::Continue;
            }
            if stack_c.visible_child_name().as_deref() == Some("temperatures") {
                let s = sensors::read_all_sensors();
                let new_temps = temperatures_page::build(&s);
                if let Some(old) = stack_c.child_by_name("temperatures") {
                    stack_c.remove(&old);
                }
                stack_c.add_titled(
                    &new_temps,
                    Some("temperatures"),
                    &crate::i18n::t("temperatures"),
                );
                stack_c.set_visible_child_name("temperatures");
            }
            glib::ControlFlow::Continue
        });
    }

    split
}

pub(crate) fn find_resource(name: &str) -> Option<String> {
    find_resource_path(name).map(|p| p.to_string_lossy().to_string())
}

fn find_resource_path(name: &str) -> Option<std::path::PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        let dir = exe.parent()?;
        let p = dir.join("../../resources").join(name);
        if p.exists() {
            return Some(p);
        }
        let p = dir.join(name);
        if p.exists() {
            return Some(p);
        }
    }
    let dev = std::path::PathBuf::from(format!("/opt/predator-sense/resources/{}", name));
    if dev.exists() {
        return Some(dev);
    }
    None
}

/// Marks `widget` and every descendant (children, grandchildren, ...) as
/// needing to redraw. `Widget::queue_draw` alone only invalidates the one
/// widget it is called on - GTK4 caches each widget's own render node and
/// reuses a child's unless something specifically told that child it
/// changed, so recoloring the live theme needs this walk to actually reach
/// every hand-drawn `DrawingArea` (sidebar highlight, gauges, the Mode
/// cards) rather than just the window's own top-level surface.
fn queue_draw_recursive(widget: &gtk::Widget) {
    widget.queue_draw();
    let mut child = widget.first_child();
    while let Some(w) = child {
        queue_draw_recursive(&w);
        child = w.next_sibling();
    }
}

fn build_settings_page(_app: &adw::Application) -> gtk::ScrolledWindow {
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroll.set_hexpand(true);
    scroll.set_vexpand(true);

    let page = gtk::Box::new(gtk::Orientation::Vertical, 16);
    page.set_margin_top(24);
    page.set_margin_bottom(24);
    page.set_margin_start(24);
    page.set_margin_end(24);

    use crate::i18n::t;
    let title = gtk::Label::new(Some(t("settings_title")));
    title.add_css_class("section-title");
    page.append(&title);

    let cfg = config::load_app_config();

    // === Supported features (auto-detected for this model) ===
    let feat_title = gtk::Label::new(Some(t("dashboard_features")));
    feat_title.add_css_class("settings-section-title");
    feat_title.set_halign(gtk::Align::Start);
    feat_title.set_margin_top(8);
    page.append(&feat_title);
    page.append(&dashboard_page::build_features_flow());

    // === Language (issue #17: no way to override the LANG/LANGUAGE-based
    // auto-detect from the UI, so a Portuguese locale always got PT-BR text
    // regardless of what the user actually reads) ===
    let lang_title = gtk::Label::new(Some(t("language")));
    lang_title.add_css_class("settings-section-title");
    lang_title.set_halign(gtk::Align::Start);
    lang_title.set_margin_top(16);
    page.append(&lang_title);

    let lang_choices: [(&str, &str); 9] = [
        ("pt", "language_pt"),
        ("en", "language_en"),
        ("es", "language_es"),
        ("zh", "language_zh"),
        ("ja", "language_ja"),
        ("ru", "language_ru"),
        ("de", "language_de"),
        ("it", "language_it"),
        ("tr", "language_tr"),
    ];
    let lang_labels: Vec<&str> = lang_choices.iter().map(|(_, k)| t(k)).collect();
    let current_lang_code = cfg.language.clone().unwrap_or_else(|| {
        match crate::i18n::lang() {
            crate::i18n::Lang::Pt => "pt",
            crate::i18n::Lang::En => "en",
            crate::i18n::Lang::Es => "es",
            crate::i18n::Lang::Zh => "zh",
            crate::i18n::Lang::Ja => "ja",
            crate::i18n::Lang::Ru => "ru",
            crate::i18n::Lang::De => "de",
            crate::i18n::Lang::It => "it",
            crate::i18n::Lang::Tr => "tr",
        }
        .to_string()
    });
    let lang_selected = lang_choices
        .iter()
        .position(|(code, _)| *code == current_lang_code)
        .unwrap_or(0) as u32;

    let lang_row = create_setting_row(t("language"), t("language_desc"));
    let lang_dd = gtk::DropDown::from_strings(&lang_labels);
    lang_dd.set_selected(lang_selected);
    lang_dd.set_valign(gtk::Align::Center);
    let current_lang_code_c = current_lang_code.clone();
    lang_dd.connect_selected_notify(move |dd| {
        let sel = dd.selected() as usize;
        if sel >= lang_choices.len() {
            return; // GTK_INVALID_LIST_POSITION or other transient state, not a real user pick
        }
        let new_lang = lang_choices[sel].0.to_string();
        if new_lang == current_lang_code_c {
            return; // re-selecting the language already active, nothing to do
        }
        let mut c = config::load_app_config();
        c.language = Some(new_lang);
        let _ = config::save_app_config(&c);

        // i18n::LANG is a OnceLock seeded once at startup, and every page is
        // built exactly once - there's no live re-render path to flip the
        // language in place. Relaunch immediately instead of leaving the user
        // staring at a dropdown that visibly did nothing: closing the window
        // alone doesn't restart the process when "minimize on close" is on
        // (confirmed this is what happened when this shipped - closing to
        // tray kept the old process, and old process kept the old language,
        // with no obvious way for the user to tell the two apart).
        //
        // The app is a single-instance GApplication (default flags, no
        // NON_UNIQUE) registered on the session D-Bus - spawning the
        // replacement before this process actually exits would just have it
        // hand off to the dying primary instance and quit immediately,
        // leaving no window at all. The replacement's typed internal argument
        // delays GTK initialization long enough for the D-Bus name to be freed.
        if let Ok(exe) = std::env::current_exe() {
            let _ = std::process::Command::new(exe)
                .arg(predator_sense_protocol::internal::DELAYED_APPLICATION_START_ARGUMENT)
                .spawn();
        }
        std::process::exit(0);
    });
    lang_row.append(&lang_dd);
    page.append(&lang_row);

    let beh_title = gtk::Label::new(Some(t("behavior")));
    beh_title.add_css_class("settings-section-title");
    beh_title.set_halign(gtk::Align::Start);
    beh_title.set_margin_top(16);
    page.append(&beh_title);

    let tray_row = create_setting_row(t("minimize_close"), t("minimize_desc"));
    let tray_switch = gtk::Switch::new();
    tray_switch.set_active(cfg.minimize_on_close);
    tray_switch.set_valign(gtk::Align::Center);
    tray_switch.connect_state_set(move |_, active| {
        let mut c = config::load_app_config();
        c.minimize_on_close = active;
        let _ = config::save_app_config(&c);
        glib::Propagation::Proceed
    });
    tray_row.append(&tray_switch);
    page.append(&tray_row);

    // Auto apply
    let auto_row = create_setting_row(t("auto_apply"), t("auto_apply_desc"));
    let auto_switch = gtk::Switch::new();
    auto_switch.set_active(cfg.auto_apply_on_start);
    auto_switch.set_valign(gtk::Align::Center);
    auto_switch.connect_state_set(move |_, active| {
        let mut c = config::load_app_config();
        c.auto_apply_on_start = active;
        let _ = config::save_app_config(&c);
        glib::Propagation::Proceed
    });
    auto_row.append(&auto_switch);
    page.append(&auto_row);

    // Custom icons (Dashboard + Temperaturas)
    let icons_row = create_setting_row(t("custom_icons"), t("custom_icons_desc"));
    let icons_switch = gtk::Switch::new();
    icons_switch.set_active(cfg.custom_icons_enabled);
    icons_switch.set_valign(gtk::Align::Center);
    icons_switch.connect_state_set(move |_, active| {
        let mut c = config::load_app_config();
        c.custom_icons_enabled = active;
        let _ = config::save_app_config(&c);
        glib::Propagation::Proceed
    });
    icons_row.append(&icons_switch);
    page.append(&icons_row);

    // Opt-out for the live per-mode recolor (see `ui::window::build`'s
    // profile watcher) - no helper/hardware write of its own, so a plain
    // `connect_state_set` is enough, same as `custom_icons_enabled` above.
    let theme_row = create_setting_row(t("keep_default_theme_color"), t("keep_default_theme_color_desc"));
    let theme_switch = gtk::Switch::new();
    theme_switch.set_active(cfg.keep_default_theme_color);
    theme_switch.set_valign(gtk::Align::Center);
    theme_switch.connect_state_set(move |_, active| {
        let mut c = config::load_app_config();
        c.keep_default_theme_color = active;
        let _ = config::save_app_config(&c);
        glib::Propagation::Proceed
    });
    theme_row.append(&theme_switch);
    page.append(&theme_row);

    // Eco Mode - caps volume/brightness at 40%, restores previous values on
    // off. Touches the privileged helper for the brightness half (see
    // `hardware::eco_mode` docs), so this runs off-thread like every other
    // helper-backed switch, not inline in `connect_state_set`.
    let eco_row = create_setting_row(t("eco_mode"), t("eco_mode_desc"));
    let eco_switch = gtk::Switch::new();
    eco_switch.set_active(cfg.eco_mode_enabled);
    eco_switch.set_valign(gtk::Align::Center);
    eco_switch.connect_state_set(move |switch, active| {
        switch.set_sensitive(false);
        let switch = switch.clone();
        background::run(
            move || {
                let mut c = config::load_app_config();
                let result = if active {
                    crate::hardware::eco_mode::enable().map(|(volume, brightness)| {
                        c.eco_mode_saved_volume_pct = volume;
                        c.eco_mode_saved_brightness_pct = brightness;
                    })
                } else {
                    let result = crate::hardware::eco_mode::disable(
                        c.eco_mode_saved_volume_pct,
                        c.eco_mode_saved_brightness_pct,
                    );
                    c.eco_mode_saved_volume_pct = None;
                    c.eco_mode_saved_brightness_pct = None;
                    result
                };
                if result.is_ok() {
                    c.eco_mode_enabled = active;
                }
                let _ = config::save_app_config(&c);
                result
            },
            move |result| {
                switch.set_sensitive(true);
                // A failure (most likely: the polkit prompt for the
                // brightness half was dismissed) puts the switch back to
                // what it actually is now, not what the click asked for -
                // `set_state` rather than `set_active` so this does not
                // re-enter `connect_state_set` and retry.
                if result.is_err() {
                    switch.set_state(!active);
                }
            },
        );
        glib::Propagation::Proceed
    });
    eco_row.append(&eco_switch);
    page.append(&eco_row);

    // Start on boot
    let boot_row = create_setting_row(t("start_on_boot"), t("start_on_boot_desc"));
    let boot_switch = gtk::Switch::new();
    boot_switch.set_active(cfg.start_on_boot);
    boot_switch.set_valign(gtk::Align::Center);
    boot_switch.connect_state_set(move |_, active| {
        let mut c = config::load_app_config();
        c.start_on_boot = active;
        let _ = config::save_app_config(&c);
        config::set_autostart(active);
        glib::Propagation::Proceed
    });
    boot_row.append(&boot_switch);
    page.append(&boot_row);

    // Critical temperature alerts
    let alert_row = create_setting_row(t("temp_alert_setting"), t("temp_alert_desc"));
    let alert_switch = gtk::Switch::new();
    alert_switch.set_active(cfg.temp_alerts);
    alert_switch.set_valign(gtk::Align::Center);
    alert_switch.connect_state_set(move |_, active| {
        let mut c = config::load_app_config();
        c.temp_alerts = active;
        let _ = config::save_app_config(&c);
        crate::hardware::alerts::set_enabled(active);
        glib::Propagation::Proceed
    });
    alert_row.append(&alert_switch);
    page.append(&alert_row);

    // Auto performance profile by power source (AC vs battery)
    let acp_row = create_setting_row(t("auto_profile_ac"), t("auto_profile_ac_desc"));
    let acp_switch = gtk::Switch::new();
    acp_switch.set_active(cfg.auto_profile_ac);
    acp_switch.set_valign(gtk::Align::Center);
    acp_row.append(&acp_switch);
    page.append(&acp_row);

    // Eco is battery-only in the official app (`MUI_Mode_Intro_ECO`, "Can be
    // used when running on battery only") - it never gets an AC card, so the
    // AC target list stays at the same four choices it always had. The
    // battery list gets it as a fifth choice.
    //
    // Dropdown position is matched against these arrays directly rather than
    // through `PowerProfile::index()`: Eco sits *below* Quiet in that
    // ordering (see its doc comment), so position and index no longer agree
    // once Eco exists, and the AC list does not even carry it at all.
    let ac_profile_choices: [(&str, crate::hardware::profile::PowerProfile); 4] = [
        ("quiet", crate::hardware::profile::PowerProfile::Quiet),
        ("balanced", crate::hardware::profile::PowerProfile::Balanced),
        (
            "performance",
            crate::hardware::profile::PowerProfile::Performance,
        ),
        ("turbo", crate::hardware::profile::PowerProfile::Turbo),
    ];
    let battery_profile_choices: [(&str, crate::hardware::profile::PowerProfile); 5] = [
        ("quiet", crate::hardware::profile::PowerProfile::Quiet),
        ("balanced", crate::hardware::profile::PowerProfile::Balanced),
        (
            "performance",
            crate::hardware::profile::PowerProfile::Performance,
        ),
        ("turbo", crate::hardware::profile::PowerProfile::Turbo),
        ("eco", crate::hardware::profile::PowerProfile::Eco),
    ];
    let ac_profile_labels: Vec<&str> = ac_profile_choices.iter().map(|(k, _)| t(k)).collect();
    let battery_profile_labels: Vec<&str> =
        battery_profile_choices.iter().map(|(k, _)| t(k)).collect();

    let ac_profile_row = create_setting_row(t("profile_when_ac"), t("profile_when_ac_desc"));
    ac_profile_row.set_sensitive(cfg.auto_profile_ac);
    let ac_profile_dd = gtk::DropDown::from_strings(&ac_profile_labels);
    let ac_selected = ac_profile_choices
        .iter()
        .position(|(_, p)| *p == cfg.profile_ac)
        .unwrap_or(1) as u32; // Balanced, if the saved choice is somehow Eco.
    ac_profile_dd.set_selected(ac_selected);
    ac_profile_dd.set_valign(gtk::Align::Center);
    ac_profile_dd.connect_selected_notify(move |dd| {
        let sel = dd.selected() as usize;
        let Some((_, profile)) = ac_profile_choices.get(sel) else {
            return; // GTK_INVALID_LIST_POSITION or other transient state, not a real user pick
        };
        let mut c = config::load_app_config();
        c.profile_ac = *profile;
        let _ = config::save_app_config(&c);
        crate::hardware::power_profile::set_target_profiles(c.profile_ac, c.profile_battery);
    });
    ac_profile_row.append(&ac_profile_dd);
    page.append(&ac_profile_row);

    let battery_profile_row =
        create_setting_row(t("profile_when_battery"), t("profile_when_battery_desc"));
    battery_profile_row.set_sensitive(cfg.auto_profile_ac);
    let battery_profile_dd = gtk::DropDown::from_strings(&battery_profile_labels);
    let battery_selected = battery_profile_choices
        .iter()
        .position(|(_, p)| *p == cfg.profile_battery)
        .unwrap_or(1) as u32; // Balanced, if the saved choice can't be shown.
    battery_profile_dd.set_selected(battery_selected);
    battery_profile_dd.set_valign(gtk::Align::Center);
    battery_profile_dd.connect_selected_notify(move |dd| {
        let sel = dd.selected() as usize;
        let Some((_, profile)) = battery_profile_choices.get(sel) else {
            return; // GTK_INVALID_LIST_POSITION or other transient state, not a real user pick
        };
        let mut c = config::load_app_config();
        c.profile_battery = *profile;
        let _ = config::save_app_config(&c);
        crate::hardware::power_profile::set_target_profiles(c.profile_ac, c.profile_battery);
    });
    battery_profile_row.append(&battery_profile_dd);
    page.append(&battery_profile_row);

    {
        let ac_row = ac_profile_row.clone();
        let bat_row = battery_profile_row.clone();
        acp_switch.connect_state_set(move |_, active| {
            let mut c = config::load_app_config();
            c.auto_profile_ac = active;
            let _ = config::save_app_config(&c);
            crate::hardware::power_profile::set_auto(active);
            ac_row.set_sensitive(active);
            bat_row.set_sensitive(active);
            glib::Propagation::Proceed
        });
    }

    // Persistent debug log (issue #7) - off by default, only meant for
    // remote debugging sessions like the one that motivated it.
    let log_row = create_setting_row(t("debug_logging"), t("debug_logging_desc"));
    let log_switch = gtk::Switch::new();
    log_switch.set_active(cfg.debug_logging);
    log_switch.set_valign(gtk::Align::Center);
    log_switch.connect_state_set(move |_, active| {
        let mut c = config::load_app_config();
        c.debug_logging = active;
        let _ = config::save_app_config(&c);
        crate::hardware::applog::set_enabled(active);
        // Best-effort: the hotkey daemon is a separate process and reads
        // this same config.json only at its own startup, so restart it to
        // pick the toggle up immediately. Harmless no-op if the service
        // isn't installed (e.g. running without the module).
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "restart", "predator-sense-hotkey.service"])
            .output();
        glib::Propagation::Proceed
    });
    log_row.append(&log_switch);
    page.append(&log_row);

    // Keep fan on Auto in Performance/Turbo (issue #41, TongkyakHermit) - off
    // by default, matches the physical Predator/Turbo key forcing Max. The
    // physical-key handler and the "Modo" page both go through
    // hardware::profile::set_profile(), which reads this same in-memory
    // flag, so no daemon restart is needed here (unlike debug_logging
    // above): the change takes effect on the very next profile switch,
    // whichever path triggers it.
    let fan_auto_row = create_setting_row(
        t("keep_fan_auto_in_performance"),
        t("keep_fan_auto_in_performance_desc"),
    );
    let fan_auto_switch = gtk::Switch::new();
    fan_auto_switch.set_active(cfg.keep_fan_auto_in_performance);
    fan_auto_switch.set_valign(gtk::Align::Center);
    fan_auto_switch.connect_state_set(move |_, active| {
        let mut c = config::load_app_config();
        c.keep_fan_auto_in_performance = active;
        let _ = config::save_app_config(&c);
        crate::hardware::profile::set_keep_fan_auto_in_performance(active);
        glib::Propagation::Proceed
    });
    fan_auto_row.append(&fan_auto_switch);
    page.append(&fan_auto_row);

    // CPU power management opt-out (issue #57, dathide) - some users run a
    // separate CPU tuning tool (e.g. `tuned` with a custom profile) that
    // writes the same governor/EPP/turbo/min_perf sysfs files a profile
    // switch does here, so the two fight over the same knobs. Same
    // no-daemon-restart-needed reasoning as fan_auto_row above: set_profile()
    // reads this in-memory flag directly, so it takes effect on the very
    // next profile switch.
    let cpu_power_row = create_setting_row(t("manage_cpu_power"), t("manage_cpu_power_desc"));
    let cpu_power_switch = gtk::Switch::new();
    cpu_power_switch.set_active(cfg.manage_cpu_power);
    cpu_power_switch.set_valign(gtk::Align::Center);
    cpu_power_switch.connect_state_set(move |_, active| {
        let mut c = config::load_app_config();
        c.manage_cpu_power = active;
        let _ = config::save_app_config(&c);
        crate::hardware::profile::set_manage_cpu_power(active);
        glib::Propagation::Proceed
    });
    cpu_power_row.append(&cpu_power_switch);
    page.append(&cpu_power_row);

    // === Accessibility Section ===
    let acc_title = gtk::Label::new(Some(t("accessibility")));
    acc_title.add_css_class("settings-section-title");
    acc_title.set_halign(gtk::Align::Start);
    acc_title.set_margin_top(20);
    page.append(&acc_title);

    let font_row = create_setting_row(t("font_scale"), t("font_scale_desc"));
    let font_scale_label = gtk::Label::new(Some(&format!(
        "{}%",
        (cfg.font_scale * 100.0).round() as i32
    )));
    font_scale_label.add_css_class("settings-row-desc");
    let font_scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 100.0, 150.0, 5.0);
    font_scale.set_value(cfg.font_scale * 100.0);
    font_scale.set_size_request(160, -1);
    font_scale.set_valign(gtk::Align::Center);
    font_scale.add_css_class("accent-scale");
    {
        let lbl = font_scale_label.clone();
        font_scale.connect_value_changed(move |sc| {
            let pct = sc.value();
            let scale = pct / 100.0;
            lbl.set_text(&format!("{}%", pct.round() as i32));
            crate::apply_font_scale(scale);
            let mut c = config::load_app_config();
            c.font_scale = scale;
            let _ = config::save_app_config(&c);
        });
    }
    font_row.append(&font_scale_label);
    font_row.append(&font_scale);
    page.append(&font_row);

    // === AI Assistant Section (opt-in) ===
    // Enable/permission toggles live here (Settings), matching every other
    // opt-in feature's pattern; the actual chat/model-manager UI lives on
    // its own page (see ui/ai_page.rs) since it needs a lot more room.
    let ai_title = gtk::Label::new(Some(t("ai_assistant_title")));
    ai_title.add_css_class("settings-section-title");
    ai_title.set_halign(gtk::Align::Start);
    ai_title.set_margin_top(20);
    page.append(&ai_title);

    let ai_desc = gtk::Label::new(Some(t("ai_assistant_desc")));
    ai_desc.add_css_class("info-note");
    ai_desc.set_halign(gtk::Align::Start);
    ai_desc.set_wrap(true);
    page.append(&ai_desc);

    let ai_enable_row = create_setting_row(t("ai_assistant_enable"), t("ai_assistant_enable_desc"));
    let ai_enable_switch = gtk::Switch::new();
    ai_enable_switch.set_active(cfg.ai_assistant_enabled);
    ai_enable_switch.set_valign(gtk::Align::Center);
    ai_enable_row.append(&ai_enable_switch);
    page.append(&ai_enable_row);

    let ai_auto_row = create_setting_row(t("ai_auto_apply"), t("ai_auto_apply_desc"));
    let ai_auto_switch = gtk::Switch::new();
    ai_auto_switch.set_active(cfg.ai_auto_apply);
    ai_auto_switch.set_valign(gtk::Align::Center);
    ai_auto_switch.set_sensitive(cfg.ai_assistant_enabled);
    ai_auto_row.append(&ai_auto_switch);
    page.append(&ai_auto_row);
    ai_auto_switch.connect_state_set(move |_, active| {
        let mut c = config::load_app_config();
        c.ai_auto_apply = active;
        let _ = config::save_app_config(&c);
        glib::Propagation::Proceed
    });

    let ai_interval_row = create_setting_row(t("ai_check_interval"), t("ai_check_interval_desc"));
    let ai_interval_spin = gtk::SpinButton::with_range(1.0, 180.0, 1.0);
    ai_interval_spin.set_value(cfg.ai_check_interval_min as f64);
    ai_interval_spin.set_valign(gtk::Align::Center);
    ai_interval_spin.set_sensitive(cfg.ai_assistant_enabled);
    ai_interval_row.append(&ai_interval_spin);
    page.append(&ai_interval_row);
    ai_interval_spin.connect_value_changed(move |sp| {
        let mut c = config::load_app_config();
        c.ai_check_interval_min = sp.value() as u32;
        let _ = config::save_app_config(&c);
    });

    {
        let ai_auto_switch = ai_auto_switch.clone();
        let ai_interval_spin = ai_interval_spin.clone();
        ai_enable_switch.connect_state_set(move |_, active| {
            let mut c = config::load_app_config();
            c.ai_assistant_enabled = active;
            let _ = config::save_app_config(&c);
            ai_auto_switch.set_sensitive(active);
            ai_interval_spin.set_sensitive(active);
            glib::Propagation::Proceed
        });
    }

    // Module status
    // === Hardware Settings Section ===
    let hw_title = gtk::Label::new(Some(t("hw_settings")));
    hw_title.add_css_class("settings-section-title");
    hw_title.set_halign(gtk::Align::Start);
    hw_title.set_margin_top(20);
    page.append(&hw_title);

    // Hardware extras are capability-gated: shown only when the machine
    // actually exposes them. Battery limit needs sysfs threshold support;
    // LCD overdrive / boot animation / USB charging need EC access (/dev/ec).
    let caps = crate::hardware::capabilities::get();
    let mut hw_any = false;

    // Battery limiter
    if caps.battery_limit {
        hw_any = true;
        let bat_row = create_setting_row(t("bat_limiter"), t("bat_limiter_desc"));
        let bat_switch = gtk::Switch::new();
        bat_switch.set_active(crate::hardware::extras::get_battery_limiter());
        bat_switch.set_valign(gtk::Align::Center);
        bat_switch.connect_state_set(|_, active| {
            let _ = crate::hardware::extras::set_battery_limiter(active);
            // Persist so it can be re-applied on boot (issue #11) - the EC
            // resets charge_control_end_threshold on a full power cycle.
            let mut cfg = config::load_app_config();
            cfg.battery_limiter = active;
            let _ = config::save_app_config(&cfg);
            glib::Propagation::Proceed
        });
        bat_row.append(&bat_switch);
        page.append(&bat_row);
    }

    if caps.ec {
        hw_any = true;
        // LCD Overdrive
        let lcd_row = create_setting_row(t("lcd_overdrive"), t("lcd_overdrive_desc"));
        let lcd_switch = gtk::Switch::new();
        lcd_switch.set_valign(gtk::Align::Center);
        lcd_switch.set_sensitive(false);
        lcd_row.append(&lcd_switch);
        page.append(&lcd_row);

        // Boot animation
        let boot_row = create_setting_row(t("boot_anim"), t("boot_anim_desc"));
        let boot_switch = gtk::Switch::new();
        boot_switch.set_valign(gtk::Align::Center);
        boot_switch.set_sensitive(false);
        boot_row.append(&boot_switch);
        page.append(&boot_row);

        // USB charging
        let usb_row = create_setting_row(t("usb_charge"), t("usb_charge_desc"));
        let usb_switch = gtk::Switch::new();
        usb_switch.set_valign(gtk::Align::Center);
        usb_switch.set_sensitive(false);
        usb_row.append(&usb_switch);
        page.append(&usb_row);

        // Keyboard backlight auto-off timer
        let backlight_timeout_row =
            create_setting_row(t("backlight_timeout"), t("backlight_timeout_desc"));
        let backlight_timeout_switch = gtk::Switch::new();
        backlight_timeout_switch.set_valign(gtk::Align::Center);
        backlight_timeout_switch.set_sensitive(false);
        backlight_timeout_row.append(&backlight_timeout_switch);
        page.append(&backlight_timeout_row);

        // These reads each launch the EC helper and can take ~150 ms. Keep
        // the switches disabled until all states arrive off-thread, then
        // attach write handlers only after setting their initial values.
        background::run(
            || {
                (
                    crate::hardware::extras::get_lcd_overdrive(),
                    crate::hardware::extras::get_boot_animation(),
                    crate::hardware::extras::get_usb_charging(),
                    crate::hardware::extras::get_backlight_timeout(),
                )
            },
            move |(lcd_enabled, boot_enabled, usb_enabled, backlight_timeout_enabled)| {
                lcd_switch.set_active(lcd_enabled);
                lcd_switch.connect_state_set(|_, active| {
                    let _ = crate::hardware::extras::set_lcd_overdrive(active);
                    glib::Propagation::Proceed
                });
                lcd_switch.set_sensitive(true);

                boot_switch.set_active(boot_enabled);
                boot_switch.connect_state_set(|_, active| {
                    let _ = crate::hardware::extras::set_boot_animation(active);
                    glib::Propagation::Proceed
                });
                boot_switch.set_sensitive(true);

                usb_switch.set_active(usb_enabled);
                usb_switch.connect_state_set(|_, active| {
                    let _ = crate::hardware::extras::set_usb_charging(active);
                    glib::Propagation::Proceed
                });
                usb_switch.set_sensitive(true);

                backlight_timeout_switch.set_active(backlight_timeout_enabled);
                backlight_timeout_switch.connect_state_set(|_, active| {
                    let _ = crate::hardware::extras::set_backlight_timeout(active);
                    glib::Propagation::Proceed
                });
                backlight_timeout_switch.set_sensitive(true);
            },
        );
    }

    // Discrete GPU mode (MUX switch), decoded from the real Windows app
    // rather than mainline - gated by the sysfs attribute's own existence,
    // which `facer.c` only creates after successfully probing the WMI call
    // at module bind (see discrete_gpu.rs). Most Predator laptops have no
    // physical MUX switch at all, so this is expected to stay hidden on
    // most machines - it is not a sign of anything broken.
    if crate::hardware::discrete_gpu::is_available() {
        hw_any = true;
        let gpu_mode_row = create_setting_row(t("discrete_gpu_mode"), t("discrete_gpu_mode_desc"));
        let gpu_mode_switch = gtk::Switch::new();
        gpu_mode_switch.set_valign(gtk::Align::Center);
        gpu_mode_switch.set_active(
            crate::hardware::discrete_gpu::get()
                == Some(crate::hardware::discrete_gpu::GpuMode::DiscreteOnly),
        );
        gpu_mode_switch.connect_state_set(|_, active| {
            let mode = if active {
                crate::hardware::discrete_gpu::GpuMode::DiscreteOnly
            } else {
                crate::hardware::discrete_gpu::GpuMode::Hybrid
            };
            let _ = crate::hardware::discrete_gpu::set(mode);
            glib::Propagation::Proceed
        });
        gpu_mode_row.append(&gpu_mode_switch);
        page.append(&gpu_mode_row);
    }

    if !hw_any {
        let note = gtk::Label::new(Some(t("hw_extras_none")));
        note.add_css_class("info-note");
        note.set_halign(gtk::Align::Start);
        note.set_wrap(true);
        page.append(&note);
    }

    // === Module Section ===
    let mod_title = gtk::Label::new(Some(t("module_kernel")));
    mod_title.add_css_class("settings-section-title");
    mod_title.set_halign(gtk::Align::Start);
    mod_title.set_margin_top(24);
    page.append(&mod_title);

    let status = setup::check_status();
    let st_text = match &status {
        setup::ModuleStatus::Ready => t("module_status_ready"),
        setup::ModuleStatus::AlternativeDriver => t("module_status_alt_driver"),
        setup::ModuleStatus::NeedsFacerInstall => t("module_status_needs_install"),
        setup::ModuleStatus::NeedsFacerLoad => t("module_status_needs_load"),
        setup::ModuleStatus::MissingDependencies(_) => t("module_status_missing_deps"),
    };
    let mod_row = create_setting_row(t("status"), st_text);
    let dot = gtk::Label::new(Some("●"));
    dot.set_valign(gtk::Align::Center);
    let dot_ok = matches!(
        status,
        setup::ModuleStatus::Ready | setup::ModuleStatus::AlternativeDriver
    );
    dot.add_css_class(if dot_ok {
        "status-dot-ok"
    } else {
        "status-dot-off"
    });
    mod_row.append(&dot);
    page.append(&mod_row);

    if let Some(serial) = crate::hardware::extras::get_serial_number() {
        let serial_row = create_setting_row(t("serial_number"), &serial);
        let copy_btn = gtk::Button::from_icon_name("edit-copy-symbolic");
        copy_btn.set_tooltip_text(Some(t("copy")));
        copy_btn.add_css_class("flat");
        copy_btn.connect_clicked(move |_btn| {
            if let Some(display) = gtk::gdk::Display::default() {
                display.clipboard().set_text(&serial);
            }
        });
        serial_row.append(&copy_btn);
        page.append(&serial_row);
    }

    if status != setup::ModuleStatus::Ready {
        let sl = gtk::Label::new(None);
        sl.add_css_class("status-label");
        let btn = gtk::Button::with_label(t("install_module"));
        btn.add_css_class("accent-button");
        btn.set_halign(gtk::Align::Start);
        btn.set_margin_top(8);
        let sl_c = sl.clone();
        btn.connect_clicked(move |b| {
            b.set_sensitive(false);
            b.set_label(crate::i18n::t("installing"));
            let results = setup::full_setup();
            if let Some(r) = results.last() {
                sl_c.set_text(&r.message);
                sl_c.add_css_class(if r.success {
                    "status-success"
                } else {
                    "status-error"
                });
                if r.success {
                    b.set_label(crate::i18n::t("installed"));
                } else {
                    b.set_sensitive(true);
                    b.set_label(crate::i18n::t("try_again"));
                }
            }
        });
        page.append(&btn);
        page.append(&sl);
    }

    let about = gtk::Label::new(Some(t("about")));
    about.add_css_class("settings-section-title");
    about.set_halign(gtk::Align::Start);
    about.set_margin_top(24);
    page.append(&about);
    let about_t = gtk::Label::new(Some(&crate::i18n::tf(
        "about_text",
        &[env!("CARGO_PKG_VERSION")],
    )));
    about_t.add_css_class("about-text");
    about_t.set_halign(gtk::Align::Start);
    page.append(&about_t);

    scroll.set_child(Some(&page));
    scroll
}

pub(crate) fn create_setting_row(title: &str, desc: &str) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row.add_css_class("settings-row");
    let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
    text.set_hexpand(true);
    let t = gtk::Label::new(Some(title));
    t.add_css_class("settings-row-title");
    t.set_halign(gtk::Align::Start);
    let d = gtk::Label::new(Some(desc));
    d.add_css_class("settings-row-desc");
    d.set_halign(gtk::Align::Start);
    // Every other long-text label in the app wraps (dashboard_page.rs's spec
    // cards, ai_desc/note a few lines below, gpu/fan/rgb page hints...) -
    // this one never did. ~15 setting rows use this, some with genuinely
    // long descriptions (ai_auto_apply_desc, keep_fan_auto_in_performance_desc),
    // so without wrap the row's natural width is the full unwrapped line,
    // which is wider than the window can give it once the control on the
    // right (switch/dropdown) is accounted for - the text just overflows/
    // gets clipped instead of reflowing, the "less responsive than
    // Dashboard" symptom reported.
    d.set_wrap(true);
    text.append(&t);
    text.append(&d);
    row.append(&text);
    row
}
