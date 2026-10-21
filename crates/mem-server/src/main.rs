//! `krystallizer` — the way the service is STARTED.
//!
//! Everything else in this crate is a library: `open(path)` builds a router and
//! hands it back, which is what the tests want and what an in-process consumer
//! wants. That is not the cross-process form ADR-0007 names, though — HTTP needs a
//! process somebody can run, watch, and stop. This is that process, and it is named
//! after the project so a process manager shows `krystallizer` rather than the
//! workspace's internal vocabulary.
//!
//! The flags are deliberately two: where the database lives, and where the door
//! listens. A service that grows configuration grows it in the config, not in a
//! command line that a process manager has to reproduce.

use std::net::SocketAddr;
use std::path::PathBuf;

/// Loopback by default: the door is reachable from the machine, and exposing it
/// beyond that is a deployment decision (`--addr 0.0.0.0:7760`), never a surprise.
const DEFAULT_ADDR: &str = "127.0.0.1:7760";

const USAGE: &str = "\
usage: krystallizer --path <db-dir> [--addr <ip:port>]

  --path <db-dir>     where the database lives (created if absent)
  --addr <ip:port>    where the HTTP door listens (default 127.0.0.1:7760)
  -h, --help          this text
";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut path: Option<PathBuf> = None;
    let mut addr: SocketAddr = DEFAULT_ADDR.parse().expect("the default address parses");

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--path" => {
                let value = args
                    .next()
                    .ok_or_else(|| format!("--path needs a directory\n{USAGE}"))?;
                path = Some(PathBuf::from(value));
            }
            "--addr" => {
                let value = args
                    .next()
                    .ok_or_else(|| format!("--addr needs <ip:port>\n{USAGE}"))?;
                addr = value
                    .parse()
                    .map_err(|e| format!("--addr `{value}`: {e}\n{USAGE}"))?;
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            other => return Err(format!("unknown argument `{other}`\n{USAGE}").into()),
        }
    }
    // A missing path is a usage error, not a default: a service that silently
    // creates a database in whatever directory it was started from is a service
    // that loses data on the next start somewhere else.
    let dir = path.ok_or_else(|| format!("--path is required\n{USAGE}"))?;

    let app = mem_server::open(dir)?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    // The bound address, not the requested one: `--addr 127.0.0.1:0` is how a test
    // or a supervisor asks the OS to pick, and it needs the answer back.
    let bound = listener.local_addr()?;
    println!("krystallizer listening on http://{bound}");
    axum::serve(listener, app).await?;
    Ok(())
}