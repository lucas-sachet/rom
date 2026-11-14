//! One-shot helper used by tests and optional installs next to `rom.exe`.

fn main() {
    let pid: u32 = match std::env::args().nth(1).and_then(|value| value.parse().ok()) {
        Some(pid) => pid,
        None => {
            eprintln!("usage: rom-ctrl-break <pid>");
            std::process::exit(2);
        }
    };
    match rom_core::console::deliver_ctrl_break(pid) {
        Ok(()) => {}
        Err(error) => {
            eprintln!("rom-ctrl-break: {error}");
            std::process::exit(1);
        }
    }
}
