//! `krystallizer` — the way the service is STARTED.
//!
//! Everything else in this crate is a library: `open(path)` builds a router and
//! hands it back, which is what the tests want and what an in-process consumer
//! wants. That is not the cross-process form ADR-0007 names, though — HTTP needs a
//! process somebody can run, watch, and stop. This is that process, and it is named
//! after the project so a process manager shows `krystallizer` rather than the
//! workspace's internal vocabulary.
//!
//! The service's parameters are ONE config file (`krystallizer.kdl`), not a command
//! line a supervisor has to reproduce: the database's directory, the embedding
//! model, the compression thresholds and the log's ceiling all live there. Two
//! flags remain, and neither states a policy: which config file (when it is not the
//! one beside the binary) and where the door listens.

use std::net::SocketAddr;
use std::path::PathBuf;

/// Loopback by default: the door is reachable from the machine, and exposing it
/// beyond that is a deployment decision (`--addr 0.0.0.0:7760`), never a surprise.
const DEFAULT_ADDR: &str = "127.0.0.1:7760";

/// The one config file's name. Its DEFAULT location is beside the binary: a
/// deployment ships the two together, so a supervisor starts `krystallizer` and the
/// process knows what it is serving without a command line kept in sync with the
/// file by hand.
const CONFIG_FILE: &str = "krystallizer.kdl";

const USAGE: &str = "\
usage: krystallizer [--config <path>] [--addr <ip:port>]

  --config <path>    the KDL config (default: krystallizer.kdl beside this binary)
  --addr <ip:port>   where the HTTP door listens (default 127.0.0.1:7760)
  -h, --help         this text

Everything else is the config: `storage.data-dir` (the database's directory,
`$HOME` allowed), the `embedding` model directory, the `compression` numbers and
the `logs` ceiling.";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut config: Option<PathBuf> = None;
    let mut addr: SocketAddr = DEFAULT_ADDR.parse().expect("the default address parses");

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                let value = args
                    .next()
                    .ok_or_else(|| format!("--config needs a path\n{USAGE}"))?;
                config = Some(PathBuf::from(value));
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
    let config_path = match config {
        Some(path) => path,
        None => default_config_path()?,
    };
    // A missing or unreadable config is a boot failure, never a fallback: every
    // directory this process writes to is stated there, and guessing one is how a
    // service ends up creating a second, invisible database.
    let cfg = mem_core::KrystallizerConfig::load(&config_path).map_err(|e| {
        format!(
            "{e}\n(krystallizer reads ONE config file: put `{CONFIG_FILE}` beside the binary \
             or pass --config <path>)"
        )
    })?;
    // Resolved here as well, because the boot line names the directory being served:
    // an operator watching a process start must see WHERE its state lives.
    let data_dir = cfg.storage.data_dir()?;

    let app = mem_server::open_from_config(&cfg)?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    // The bound address, not the requested one: `--addr 127.0.0.1:0` is how a test
    // or a supervisor asks the OS to pick, and it needs the answer back.
    let bound = listener.local_addr()?;
    println!("krystallizer listening on http://{bound}");
    println!("  config {}", config_path.display());
    println!("  data   {}", data_dir.display());
    axum::serve(listener, app).await?;
    Ok(())
}

/// The config read when no `--config` names one: the file beside this binary.
///
/// `current_exe` is canonicalized because a supervisor starts the binary through a
/// symlink (a store path, a `bin/` link) — the config ships beside the REAL
/// executable, not beside every link pointing at it.
fn default_config_path() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let exe = std::fs::canonicalize(std::env::current_exe()?)
        .map_err(|e| format!("cannot resolve this binary's own path: {e}"))?;
    let dir = exe
        .parent()
        .ok_or_else(|| format!("this binary has no directory: {}", exe.display()))?;
    Ok(dir.join(CONFIG_FILE))
}
