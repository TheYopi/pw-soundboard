mod app;
mod audio;
mod config;
mod decode;
mod hotkeys;
mod remote;
mod shell;
mod theme;

use std::cell::RefCell;
use std::rc::Rc;

/// eframe owns the app it runs and drops it when the window closes; this wrapper lets the real
/// app outlive the window so it can keep running in the tray.
struct Window(Rc<RefCell<app::SoundboardApp>>);

impl eframe::App for Window {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        eframe::App::ui(&mut *self.0.borrow_mut(), ui, frame);
    }
}

fn run_window(app: &Rc<RefCell<app::SoundboardApp>>) -> eframe::Result {
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../assets/pw-soundboard.png")).ok();
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("PW Soundboard")
        .with_app_id("pw-soundboard") // matches the .desktop file for Wayland compositors
        .with_inner_size([1000.0, 680.0])
        .with_min_inner_size([520.0, 360.0])
        .with_drag_and_drop(true);
    if let Some(icon) = icon {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport,
        // Return from run_native when the window closes, so it can be reopened later.
        run_and_return: true,
        ..Default::default()
    };
    let app = app.clone();
    eframe::run_native(
        "PW Soundboard",
        options,
        Box::new(move |cc| {
            app.borrow_mut().attach(&cc.egui_ctx);
            Ok(Box::new(Window(app)))
        }),
    )
}

fn main() -> eframe::Result {
    // `--play 3` etc. (used by keyboard shortcuts) talk to the running app and exit right away.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(code) = remote::run_cli(&args) {
        std::process::exit(code);
    }
    // Launching again while running (e.g. in the tray) just shows the existing window.
    if remote::show_existing_instance() {
        return Ok(());
    }
    let start_hidden_flag = std::env::args().any(|a| a == "--hidden");

    let (shell, events) = shell::Shell::new();
    let app = Rc::new(RefCell::new(app::SoundboardApp::new(shell.clone(), events)));
    let tray = shell::spawn_tray(shell);
    app.borrow_mut().set_tray_available(tray.is_some());

    let mut show = !(tray.is_some() && start_hidden_flag) && !app.borrow().start_hidden();
    loop {
        if show {
            run_window(&app)?;
            if !app.borrow_mut().detach() {
                break;
            }
        }
        show = app.borrow_mut().run_hidden();
        if !show {
            break;
        }
    }

    app.borrow_mut().save_now();
    if let Some(tray) = tray {
        tray.shutdown().wait();
    }
    // Dropping the app stops the audio thread (removing our PipeWire nodes) and the socket.
    drop(app);
    Ok(())
}
