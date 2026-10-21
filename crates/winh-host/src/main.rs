#[cfg(windows)]
mod broker;
#[cfg(windows)]
mod control;
#[cfg(any(windows, test))]
mod input_guard;
#[cfg(windows)]
mod install;
#[cfg(any(windows, test))]
mod revision;
#[cfg(windows)]
mod status;
#[cfg(windows)]
mod terminal;

#[cfg(windows)]
fn main() {
    let result =
        control::command().and_then(|handled| if handled { Ok(()) } else { broker::run() });
    if let Err(error) = result {
        status::startup_error(&error.to_string());
        eprintln!("错误：{error}");
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("请在 Windows 运行。");
}
