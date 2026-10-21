#![cfg_attr(windows, windows_subsystem = "windows")]
#[cfg(windows)]
mod app;
#[cfg(windows)]
mod menu;
mod model;
mod update;
#[cfg(windows)]
mod view;

fn main() {
    #[cfg(windows)]
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "--quit")
    {
        if let Err(error) = app::quit() {
            eprintln!("{error}");
            std::process::exit(1);
        }
    } else {
        app::run();
    }
    #[cfg(not(windows))]
    eprintln!("请在 Windows 运行。");
}
