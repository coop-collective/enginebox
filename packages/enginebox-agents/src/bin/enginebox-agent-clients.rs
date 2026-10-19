fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.iter().any(|a| a == "--version") {
        println!("enginebox-agent-clients {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "enginebox-agent-clients {}\nInstall official native clients in the controller service user's private directory.\n\nUsage: enginebox-agent-clients --state-dir PATH [--provider codex|claude|cursor] [--plan|--apply]\n\nDefaults to a read-only plan. --apply must run as the unprivileged controller user. Existing foreign binaries are never replaced. Sign in later through the Agents dashboard.",
            env!("CARGO_PKG_VERSION")
        );
        return;
    }
    match enginebox_agents::clients::main(&args) {
        Ok(value) => println!("{}", serde_json::to_string_pretty(&value).unwrap()),
        Err(error) => {
            eprintln!("enginebox-agent-clients: {error}");
            std::process::exit(1);
        }
    }
}
