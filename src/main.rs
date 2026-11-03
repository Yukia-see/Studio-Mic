mod app;
mod audio;
mod autostart;
mod config;
mod ipc;
mod models;
mod routing;
mod tray;

use app::{Controller, UiPush};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use tao::{
    event::{Event, StartCause, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder},
    window::WindowBuilder,
};
use wry::WebViewBuilder;

#[derive(Debug, Clone)]
pub enum AppEvent {
    Ui(UiPush),
}

const BACKGROUND_FLAGS: [&str; 5] = ["--background", "--start-minimized", "--tray", "-b", "--daemon"];

/// Stops Studio Mic and removes everything it created in PipeWire (used by the uninstaller).
fn cleanup_pipewire() {
    if ipc::instance_running() || ipc::instance_locked() {
        let _ = ipc::send(&["--quit".to_string()]);
        let start = std::time::Instant::now();
        while ipc::instance_locked() && start.elapsed() < std::time::Duration::from_secs(5) {
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
    }
    let killed = audio::daemon::cleanup_stale_processes();
    let mut removed = 0;
    for n in audio::pw::nodes() {
        let lname = n.name.to_lowercase();
        // Also matches the pre-v0.2 studiomic_processed_mic alias.
        if lname.starts_with("studiomic_") {
            if audio::pw::destroy_node(n.id) {
                removed += 1;
            }
        }
    }
    let _ = std::fs::remove_file(ipc::socket_path());
    let _ = std::fs::remove_file(audio::daemon::PREVIEW_WAV);
    println!("Studio Mic PipeWire cleanup: {} processes stopped, {} nodes removed", killed, removed);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--help" || a == "-h") {
        ipc::print_help();
        return;
    }
    if args.iter().any(|a| a == "--cleanup-pipewire") {
        cleanup_pipewire();
        return;
    }
    if let Some(pos) = args.iter().position(|a| ipc::CLI_COMMANDS.contains(&a.as_str())) {
        std::process::exit(ipc::run_client(&args[pos..]));
    }

    let background = args.iter().any(|a| BACKGROUND_FLAGS.contains(&a.as_str()));

    // Single instance: a second launch just focuses the running window. The lock file is the
    // authority (it works even if the control socket is broken); the socket is used to signal.
    let Some(_instance_lock) = ipc::acquire_instance_lock() else {
        if !background && ipc::send(&["--show".to_string()]).is_some() {
            println!("Studio Mic is already running; brought it to the front.");
        } else {
            println!("Studio Mic is already running.");
        }
        return;
    };

    if let Err(e) = run(args, background) {
        eprintln!("Studio Mic failed to start: {}", e);
        std::process::exit(1);
    }
}

fn run(args: Vec<String>, background: bool) -> Result<(), Box<dyn std::error::Error>> {
    println!("Starting Studio Mic...");

    // 1. State (migrates ~/.config/studiomic/config.json on first boot).
    let mut cfg = config::StudioMicConfig::load();
    if cfg.active_hw_mic.is_none() {
        // First boot only: pick the system's real default mic (never our own nodes).
        let mics = audio::device_discovery::DeviceDiscovery::get_input_devices();
        cfg.active_hw_mic = mics.iter().find(|d| d.is_default).or_else(|| mics.first()).map(|d| d.name.clone());
        let _ = cfg.write_atomic();
    }
    let state = Arc::new(Mutex::new(cfg.clone()));

    // 2. Audio engine (reuses existing Tier-1 buses, cleans up stale children).
    let engine = audio::daemon::Engine::start(state.clone());
    engine.ui_visible.store(!background, Ordering::SeqCst);
    let controller = Controller::new(state.clone(), engine.clone());

    // 3. Event loop + proxy plumbing.
    let event_loop = EventLoopBuilder::<AppEvent>::with_user_event().build();
    let proxy = Arc::new(Mutex::new(event_loop.create_proxy()));
    {
        let p = proxy.clone();
        controller.set_ui_sink(move |push| {
            let _ = p.lock().unwrap().send_event(AppEvent::Ui(push));
        });
    }

    // 4. Control socket for the CLI / StreamController.
    if let Err(e) = ipc::start_server(controller.clone()) {
        eprintln!("Warning: control socket unavailable: {}", e);
    }

    // 5. SIGINT / SIGTERM -> clean quit (state flushed, children stopped).
    {
        let p = proxy.clone();
        let eng = engine.clone();
        let st = state.clone();
        std::thread::Builder::new().name("signals".into()).spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(_) => return,
            };
            rt.block_on(async {
                use tokio::signal::unix::{signal, SignalKind};
                let (Ok(mut term), Ok(mut int)) = (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) else { return };
                tokio::select! {
                    _ = term.recv() => {},
                    _ = int.recv() => {},
                }
            });
            println!("Signal received: shutting down.");
            let _ = p.lock().unwrap().send_event(AppEvent::Ui(UiPush::Quit));
            // If the event loop is wedged, still leave PipeWire clean.
            std::thread::sleep(std::time::Duration::from_secs(4));
            let _ = st.lock().unwrap().write_atomic();
            eng.shutdown();
            let _ = std::fs::remove_file(ipc::socket_path());
            std::process::exit(0);
        })?;
    }

    // 6. Autostart entry follows the saved preference.
    if cfg.autostart && !autostart::AutostartManager::is_enabled() {
        let _ = autostart::AutostartManager::set_enabled_with_minimized(true, cfg.autostart_minimized);
    }

    // 7. Tray.
    let tray_handle = {
        let p = proxy.clone();
        tray::spawn_tray(controller.clone(), move |push| {
            let _ = p.lock().unwrap().send_event(AppEvent::Ui(push));
        })
        .ok()
    };

    // 8. Window + webview.
    let window = WindowBuilder::new()
        .with_title("Studio Mic")
        .with_inner_size(tao::dpi::LogicalSize::new(cfg.window_width, cfg.window_height))
        .with_resizable(true)
        .with_visible(!background)
        .build(&event_loop)?;

    let html_content = include_str!("../mic-studio-mockup.html");
    let target_tab = if args.iter().any(|a| a == "--eq") {
        "tab-eq".to_string()
    } else if args.iter().any(|a| a == "--noise") {
        "tab-noise".to_string()
    } else if args.iter().any(|a| a == "--calib") {
        "tab-calib".to_string()
    } else {
        cfg.last_tab.clone()
    };

    use audio::device_discovery::DeviceDiscovery;
    use routing::wireplumber_hook::StreamWatcher;
    let j = |v: serde_json::Result<String>| v.unwrap_or_else(|_| "null".into());
    let plugins = engine.plugins_json();
    // The startup scans are independent (PipeWire queries, .desktop files): run them in parallel
    // so the window appears sooner.
    let (mics, cards, outs, players, browsers, tabs, apps, streams) = std::thread::scope(|sc| {
        let devices = sc.spawn(|| {
            let mics = DeviceDiscovery::get_input_devices();
            let cards = DeviceDiscovery::get_capture_cards_from(&mics);
            (mics, cards, DeviceDiscovery::get_output_devices())
        });
        let players = sc.spawn(StreamWatcher::scan_installed_media_players);
        let browsers = sc.spawn(StreamWatcher::scan_installed_web_browsers);
        let apps = sc.spawn(StreamWatcher::scan_installed_apps);
        let live = sc.spawn(|| (StreamWatcher::scan_active_browser_tabs(), StreamWatcher::scan_live_streams()));
        let (mics, cards, outs) = devices.join().unwrap_or_default();
        let (tabs, streams) = live.join().unwrap_or_default();
        (
            mics,
            cards,
            outs,
            players.join().unwrap_or_default(),
            browsers.join().unwrap_or_default(),
            tabs,
            apps.join().unwrap_or_default(),
            streams,
        )
    });
    let init_script = format!(
        "window.__INITIAL_TAB__ = {tab};\n\
         window.__SAVED_CONFIG__ = {cfg};\n\
         window.__INITIAL_HARDWARE_MICS__ = {mics};\n\
         window.__INITIAL_OUTPUT_DEVICES__ = {outs};\n\
         window.__INITIAL_CAPTURE_CARDS__ = {cards};\n\
         window.__INITIAL_INSTALLED_PLAYERS__ = {players};\n\
         window.__INITIAL_INSTALLED_BROWSERS__ = {browsers};\n\
         window.__INITIAL_BROWSER_TABS__ = {tabs};\n\
         window.__INITIAL_INSTALLED_APPS__ = {apps};\n\
         window.__INITIAL_LIVE_STREAMS__ = {streams};\n\
         window.__INITIAL_AUTOSTART__ = {autostart};\n\
         window.__PLUGINS__ = {plugins};\n\
         window.__PROFILES__ = {profiles};\n\
         window.__PREVIEW_AVAILABLE__ = {preview};\n\
         window.__LATENCY__ = {latency};\n\
         document.documentElement.setAttribute('data-theme', {theme});",
        tab = j(serde_json::to_string(&target_tab)),
        cfg = j(serde_json::to_string(&*state.lock().unwrap())),
        mics = j(serde_json::to_string(&mics)),
        outs = j(serde_json::to_string(&outs)),
        cards = j(serde_json::to_string(&cards)),
        players = j(serde_json::to_string(&players)),
        browsers = j(serde_json::to_string(&browsers)),
        tabs = j(serde_json::to_string(&tabs)),
        apps = j(serde_json::to_string(&apps)),
        streams = j(serde_json::to_string(&streams)),
        autostart = autostart::AutostartManager::is_enabled(),
        plugins = plugins,
        profiles = j(serde_json::to_string(&config::StudioMicConfig::list_profiles())),
        preview = engine.preview_available(),
        // Written by tools/latency-probe (processed_ms / raw_ms) for the monitor-mode badges.
        latency = std::fs::read_to_string(config::cache_dir().join("latency.json")).ok().filter(|t| serde_json::from_str::<serde_json::Value>(t).is_ok()).unwrap_or_else(|| "{}".into()),
        theme = j(serde_json::to_string(&cfg.theme)),
    );

    // UI messages are handled on their own thread, in order. Several (device refresh, live app
    // list, preview) query PipeWire for 50-300 ms; on the GTK thread that froze the window.
    let (ui_tx, ui_rx) = std::sync::mpsc::channel::<String>();
    {
        let c = controller.clone();
        std::thread::Builder::new().name("ui-messages".into()).spawn(move || {
            for body in ui_rx {
                c.handle_ui(&body);
            }
        })?;
    }
    let ui_tx = Mutex::new(ui_tx);
    let builder = WebViewBuilder::new()
        .with_html(html_content)
        .with_initialization_script(&init_script)
        .with_ipc_handler(move |req| {
            let _ = ui_tx.lock().unwrap().send(req.body().to_string());
        });

    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "ios", target_os = "android")))]
    let webview = {
        use tao::platform::unix::WindowExtUnix;
        use wry::WebViewBuilderExtUnix;
        let vbox = window.default_vbox().expect("Failed to get default vbox on Linux");
        builder.build_gtk(vbox)?
    };
    #[cfg(any(target_os = "windows", target_os = "macos", target_os = "ios", target_os = "android"))]
    let webview = builder.build(&window)?;

    // Live stream / browser-tab list for the Active Apps pickers (only while visible).
    {
        let c = controller.clone();
        let e = engine.clone();
        std::thread::Builder::new().name("stream-list".into()).spawn(move || {
            let mut last = String::new();
            loop {
                std::thread::sleep(std::time::Duration::from_millis(2500));
                if !e.ui_visible.load(Ordering::SeqCst) {
                    continue;
                }
                // One scan per pass, reused for the change check and the push.
                let streams = StreamWatcher::scan_live_streams();
                let tabs = StreamWatcher::scan_active_browser_tabs();
                let key = serde_json::to_string(&streams).unwrap_or_default() + &serde_json::to_string(&tabs).unwrap_or_default();
                if key != last {
                    last = key;
                    c.push_live_streams_data(&streams, &tabs);
                }
            }
        })?;
    }

    println!("Studio Mic UI ready{}.", if background { " (background)" } else { "" });

    let shutdown_engine = engine.clone();
    let shutdown_state = state.clone();
    let mut shut_down = false;
    let mut do_shutdown = move || {
        if shut_down {
            return;
        }
        shut_down = true;
        let _ = shutdown_state.lock().unwrap().write_atomic();
        config::flush_now();
        shutdown_engine.shutdown();
        let _ = std::fs::remove_file(ipc::socket_path());
    };

    // Normal window size while the widget is shown (restored when it's left).
    let mut widget_saved: Option<tao::dpi::LogicalSize<f64>> = None;
    let start_widget = cfg.widget_mode;
    let widget_controller = controller.clone();

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        let set_visible = |vis: bool| {
            engine.ui_visible.store(vis, Ordering::SeqCst);
            window.set_visible(vis);
            if vis {
                window.set_focus();
            }
            let _ = webview.evaluate_script(&format!("window.setWindowVisible && window.setWindowVisible({})", vis));
        };
        match event {
            Event::NewEvents(StartCause::Init) => {
                if start_widget {
                    widget_controller.push_widget();
                }
            }
            Event::UserEvent(AppEvent::Ui(push)) => match push {
                UiPush::Js(script) => {
                    let _ = webview.evaluate_script(&script);
                }
                UiPush::ShowWindow => set_visible(true),
                UiPush::ToggleWindow => set_visible(!window.is_visible()),
                UiPush::TrayRefresh => {
                    if let Some(h) = &tray_handle {
                        let _ = h.update(|_| {});
                    }
                }
                UiPush::Widget { on, width, height, on_top } => {
                    hyprland_widget(on, width, height, on_top);
                    if on {
                        if widget_saved.is_none() {
                            widget_saved = Some(window.inner_size().to_logical::<f64>(window.scale_factor()));
                        }
                        window.set_title(WIDGET_TITLE);
                        window.set_inner_size(tao::dpi::LogicalSize::new(width, height));
                        window.set_always_on_top(on_top);
                    } else if let Some(size) = widget_saved.take() {
                        window.set_title("Studio Mic");
                        window.set_always_on_top(false);
                        window.set_inner_size(size);
                    }
                    let _ = webview.evaluate_script(&format!("window.setWidgetMode && window.setWidgetMode({})", on));
                }
                UiPush::Quit => {
                    println!("Quitting Studio Mic...");
                    window.set_visible(false);
                    do_shutdown();
                    *control_flow = ControlFlow::Exit;
                }
            },
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => {
                // Closing the window keeps the audio engine running in the tray.
                set_visible(false);
                let _ = state.lock().unwrap().write_atomic();
            }
            Event::WindowEvent { event: WindowEvent::Resized(size), .. } => {
                let logical = size.to_logical::<f64>(window.scale_factor());
                if widget_saved.is_some() {
                    // Widget resized by the user: remember its size, not the main window's.
                    let mut s = state.lock().unwrap();
                    if logical.width > 100.0 && ((s.widget_width - logical.width).abs() > 1.0 || (s.widget_height - logical.height).abs() > 1.0) {
                        s.widget_width = logical.width.round();
                        s.widget_height = logical.height.round();
                        s.normalize();
                        let _ = s.save();
                    }
                } else if logical.width > 100.0 && logical.height > 100.0 {
                    let mut s = state.lock().unwrap();
                    if (s.window_width - logical.width).abs() > 1.0 || (s.window_height - logical.height).abs() > 1.0 {
                        s.window_width = logical.width.round();
                        s.window_height = logical.height.round();
                        s.normalize();
                        let _ = s.save();
                    }
                }
            }
            Event::LoopDestroyed => do_shutdown(),
            _ => {}
        }
    });
}

const WIDGET_TITLE: &str = "Studio Mic Widget";

/// Tiled Hyprland windows ignore client resizes and on-top hints, so the widget also asks the
/// compositor directly (by our PID): float + pin + exact size. Leaving it unpins and re-tiles,
/// unless the main window was already floating before. No-op outside Hyprland.
fn hyprland_widget(on: bool, width: f64, height: f64, on_top: bool) {
    use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
    static WAS_FLOATING: AtomicBool = AtomicBool::new(false);
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
        return;
    }
    // Read before the title changes, so a Hyprland window rule on the widget title can't hide
    // the main window's own state.
    if on {
        WAS_FLOATING.store(hypr_client().is_some_and(|c| c.0), SeqCst);
    }
    std::thread::spawn(move || {
        // Let the title change reach the compositor first.
        std::thread::sleep(std::time::Duration::from_millis(150));
        let Some((floating, pinned, address)) = hypr_client() else { return };
        let w = format!("address:{}", address);
        if on {
            if !floating {
                hypr_dispatch(&format!("hl.dsp.window.float({{ window = \"{}\", action = \"enable\" }})", w), &["setfloating", &w]);
            }
            // `pin` toggles.
            if on_top != pinned {
                hypr_dispatch(&format!("hl.dsp.window.pin({{ window = \"{}\" }})", w), &["pin", &w]);
            }
            let (x, y) = (width.round() as i64, height.round() as i64);
            hypr_dispatch(
                &format!("hl.dsp.window.resize({{ window = \"{}\", x = {}, y = {} }})", w, x, y),
                &["resizewindowpixel", &format!("exact {} {},{}", x, y, w)],
            );
        } else {
            if pinned {
                hypr_dispatch(&format!("hl.dsp.window.pin({{ window = \"{}\" }})", w), &["pin", &w]);
            }
            if floating && !WAS_FLOATING.load(SeqCst) {
                hypr_dispatch(&format!("hl.dsp.window.float({{ window = \"{}\", action = \"disable\" }})", w), &["settiled", &w]);
            }
        }
    });
}

/// Hyprland 0.53+ takes Lua dispatches (`hl.dsp.window.*`); older versions the classic
/// dispatcher names. Try Lua first, fall back (same approach as omarchy-hyprland-window-pop).
fn hypr_dispatch(lua: &str, legacy: &[&str]) {
    let ok = std::process::Command::new("hyprctl")
        .args(["dispatch", lua])
        .output()
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "ok")
        .unwrap_or(false);
    if !ok {
        let _ = std::process::Command::new("hyprctl")
            .arg("dispatch")
            .args(legacy)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// (floating, pinned, address) of this process's Hyprland window.
fn hypr_client() -> Option<(bool, bool, String)> {
    std::process::Command::new("hyprctl")
        .args(["clients", "-j"])
        .output()
        .ok()
        .and_then(|o| serde_json::from_slice::<serde_json::Value>(&o.stdout).ok())
        .and_then(|v| {
            v.as_array()?.iter().find(|c| c["pid"].as_u64() == Some(std::process::id() as u64)).map(|c| {
                (c["floating"].as_bool().unwrap_or(false), c["pinned"].as_bool().unwrap_or(false), c["address"].as_str().unwrap_or("").to_string())
            })
        })
        .filter(|c| !c.2.is_empty())
}
