fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.iter().any(|a| a == "--version") {
        println!("enginebox-browser {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "enginebox-browser {}\nRun the official Playwright MCP with a private profile.\n\nOptions: --client NAME --profile NAME --headed --show-command",
            env!("CARGO_PKG_VERSION")
        );
        return;
    }
    if let Err(error) = enginebox_agents::browser::run(&args) {
        eprintln!("enginebox-browser: {error}");
        std::process::exit(1);
    }
}
