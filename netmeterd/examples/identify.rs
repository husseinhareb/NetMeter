fn main() {
    for arg in std::env::args().skip(1) {
        if let Ok(pid) = arg.parse::<u32>() {
            let exe = std::fs::read_link(format!("/proc/{pid}/exe"));
            let cmd = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
            let args: Vec<String> = cmd
                .split(|b| *b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect();
            println!("pid {pid}");
            println!("  exe      {exe:?}");
            println!("  argv     {:?}", args.iter().take(3).collect::<Vec<_>>());
            if let Ok(e) = &exe {
                println!("  specific {:?}", netmeterd::app::specific_name(e, &args));
            }
            println!("  identify {}", netmeterd::app::identify(pid, "unknown"));
        }
    }
}
