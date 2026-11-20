//! umsp binary entry point. All logic lives in the library (`src/lib.rs`).

use clap::Parser;
use umsp::{cli, commands};

fn main() {
    let cli = cli::Cli::parse();
    let json = cli.json;
    let code = match commands::run(cli) {
        Ok(code) => code,
        Err(err) => {
            if json {
                println!("{}", err.to_json());
            } else {
                eprintln!("{err}");
            }
            err.exit
        }
    };
    // Flush stdout before exiting so `serve`'s ready line is never lost.
    use std::io::Write;
    let _ = std::io::stdout().flush();
    std::process::exit(code);
}
