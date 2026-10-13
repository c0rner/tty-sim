#[cfg(target_os = "windows")]
compile_error!("tty-sim does not support Windows; use a POSIX-style target");

fn main() -> std::process::ExitCode {
    tty_sim::cmd::main_entry()
}
