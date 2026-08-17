//! cordis-cli entry point: parses command-line arguments and starts the
//! runtime.

fn main() -> anyhow::Result<()> {
    let mut config = None;
    let mut plugins_dir = None;
    let mut create_dir = None;
    let mut force = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "create" => {
                create_dir = args.next();
            }
            "--force" | "-f" => {
                force = true;
            }
            "-c" | "--config" => {
                config = args.next();
            }
            "--plugins-dir" => {
                plugins_dir = args.next();
            }
            "-h" | "--help" => {
                println!(
                    "cordis {} — usage: cordis create <name> | cordis [-c cordis.yml] [--plugins-dir plugins]",
                    env!("CARGO_PKG_VERSION")
                );
                return Ok(());
            }
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }

    if let Some(dir) = create_dir {
        return cordis_cli::create_project(std::path::Path::new(&dir), force);
    }

    let options = cordis_cli::CliOptions {
        config,
        plugins_dir,
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let local = tokio::task::LocalSet::new();
    local.block_on(&runtime, async { cordis_cli::run(&options).await })?;
    Ok(())
}
