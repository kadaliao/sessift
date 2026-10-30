use std::time::Duration;

use clap::Parser;

fn main() {
    let cli = sessift::cli::Cli::parse();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("start the async runtime");
    let code = runtime
        .block_on(sessift::cli::run(cli))
        .unwrap_or_else(|err| {
            eprintln!("sessift: {err:#}");
            1
        });
    // Blocking stdin readers would otherwise keep the runtime alive.
    runtime.shutdown_timeout(Duration::from_millis(100));
    std::process::exit(code);
}
