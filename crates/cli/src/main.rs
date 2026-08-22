use concerto_cli::run_cli;

struct CliConfig {
    multi_agent: bool,
    fast: bool,
    reconfigure: bool,
}

fn parse_args() -> CliConfig {
    let (multi_agent, fast, reconfigure, _remaining) =
        concerto_cli::parse_cli_args(std::env::args().skip(1).collect::<Vec<_>>().iter());
    CliConfig { multi_agent, fast, reconfigure }
}

fn main() -> anyhow::Result<()> {
    let config = parse_args();

    if config.multi_agent {
        tracing::info!("multi-agent mode enabled");
    }

    run_cli(config.multi_agent, config.fast, config.reconfigure)
}
