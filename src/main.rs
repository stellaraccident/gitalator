use clap::{Parser, Subcommand};
use std::path::PathBuf;
#[derive(Parser)]
struct Cli {
    #[arg(long,default_value_os_t=gitalator::store::default_root())]
    state_dir: PathBuf,
    #[command(subcommand)]
    command: Cmd,
}
#[derive(Subcommand)]
enum Cmd {
    Create {
        name: String,
        depot: String,
        #[arg(long, default_value = "HEAD")]
        revision: String,
    },
    List,
}
fn main() {
    if let Err(e) = run() {
        eprintln!("gtl: {e}");
        std::process::exit(1);
    }
}
fn run() -> gitalator::Result<()> {
    let c = Cli::parse();
    let mut s = gitalator::store::Store::open(&c.state_dir)?;
    match c.command {
        Cmd::Create {
            name,
            depot,
            revision,
        } => println!("{}", s.create(&name, &depot, &revision)?),
        Cmd::List => {
            for n in s.workspaces.keys() {
                println!("{n}");
            }
        }
    }
    Ok(())
}
