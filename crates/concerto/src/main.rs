fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // ── --help / -h  (must work without provider config) ──────────────
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return;
    }

    // ── --version / -V ────────────────────────────────────────────────
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("concerto {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    // ── Mode selection ────────────────────────────────────────────────
    //  • --cli / -c        → force CLI
    //  • --desktop / -d    → force desktop (error if desktop feature off)
    //  • (no flag)         → desktop if available, else CLI
    let force_cli = args.iter().any(|a| a == "--cli" || a == "-c");
    let force_desktop = args.iter().any(|a| a == "--desktop" || a == "-d");
    #[cfg(feature = "cli")]
    let cli_command = concerto_cli::has_cli_subcommand(&args);

    if force_cli && force_desktop {
        eprintln!("error: --cli and --desktop are mutually exclusive");
        std::process::exit(1);
    }
    #[cfg(feature = "cli")]
    if force_desktop && cli_command {
        eprintln!("error: CLI commands cannot be combined with --desktop");
        std::process::exit(1);
    }

    // ── CLI path ──────────────────────────────────────────────────────
    #[cfg(feature = "cli")]
    {
        let run_cli = force_cli || cli_command || {
            #[cfg(feature = "desktop")]
            {
                // Open desktop when no CLI command or mode flag is supplied.
                false
            }
            #[cfg(not(feature = "desktop"))]
            {
                // Only CLI feature compiled in — use it.
                true
            }
        };

        if run_cli {
            let multi_agent = args.iter().any(|a| a == "--multi-agent" || a == "-m");
            let fast = args.iter().any(|a| a == "--fast" || a == "-f");
            let reconfigure = args.iter().any(|a| a == "--reconfigure" || a == "-r");
            // `--reduced-motion` is parsed here exactly like the flags above:
            // `Some(true)` only when the flag is passed (there is no
            // `--no-reduced-motion`), `None` defers to CONCERTO_REDUCED_MOTION
            // env → `[display] reduced_motion` config → default (false) — the
            // same contract as `concerto_cli::parse_cli_args`.
            let reduced_motion = args.iter().any(|a| a == "--reduced-motion").then_some(true);
            if let Err(e) = concerto_cli::run_cli(multi_agent, fast, reconfigure, reduced_motion) {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
            return;
        }
    }

    // ── Desktop path ──────────────────────────────────────────────────
    #[cfg(feature = "desktop")]
    {
        if force_desktop || !force_cli {
            if let Err(e) = concerto_desktop::run() {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
            return;
        }
    }

    // ── Neither feature compiled ──────────────────────────────────────
    #[cfg(not(any(feature = "cli", feature = "desktop")))]
    {
        eprintln!("error: concerto requires at least one of the 'cli' or 'desktop' features");
        std::process::exit(1);
    }

    // Unreachable when at least one feature is enabled, but guard just in case.
    eprintln!("error: no runtime mode available (compile with 'cli' or 'desktop' feature)");
    std::process::exit(1);
}

fn print_help() {
    println!(
        "concerto {} — local-first AI coding agent

USAGE:
    concerto [OPTIONS]

OPTIONS:
    -c, --cli              Run the terminal (ratatui) interface
    -d, --desktop          Run the desktop (Iced) GUI interface
    -m, --multi-agent      Enable multi-agent orchestration (CLI only)
    -f, --fast             Skip memory retrieval for trivial tasks (CLI only)
    -r, --reconfigure      Re-run the setup wizard (CLI only)
    -p, --project <DIR>    Select a project for CLI commands and chat
        --theme <NAME>    Select the CLI palette
        --reduced-motion  Disable motion cues
        --no-terminal-title  Disable terminal title broadcasts
    -V, --version          Print version
    -h, --help             Print this help

DEFAULT BEHAVIOR:
    If neither --cli nor --desktop is specified, the desktop GUI is launched
    when the 'desktop' feature is enabled.  If only the 'cli' feature is
    compiled, the terminal interface is used automatically.

CLI SUBCOMMANDS:
    concerto --cli projects <list|current|use>
    concerto --cli sessions <list|show|events|resume>
    concerto --cli providers list
    concerto --cli logs <path|show>
    concerto --cli config <show|get|keys|set|unset|path|init|doctor|help>
    concerto --cli agents <list|show|import|set|clone|remove>
    concerto --cli blueprint <list|show|select|import>
    concerto --cli providers <list|add|remove|refresh>
    concerto --cli credentials <status|set|delete>
    concerto --cli preferences <show|set ui_theme NAME|set ui_font_size SIZE>
    concerto --cli extensions <list|skills ACTION|mcp ACTION>
    concerto --cli plugin <list|installed|install|remove|revoke>
    concerto --cli shell <list|test|select|managed ACTION>

CLI commands automatically select CLI mode when the 'cli' feature is enabled.
Run `concerto config help` for settings scopes, typed values, and dry runs.",
        env!("CARGO_PKG_VERSION")
    );
}
