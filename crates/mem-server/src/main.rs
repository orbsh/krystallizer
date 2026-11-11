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
       krystallizer --config <path> --restore <target> [--map <table>=<ns>]...

  --config <path>    the KDL config (default: krystallizer.kdl beside this binary)
  --addr <ip:port>   where the HTTP door listens (default 127.0.0.1:7760)
  --restore <target> land a lake back into storage.data-dir, then exit
                     (<target> is a directory or s3://bucket/prefix)
  --map <table>=<ns> restore only this table, into namespace <ns> (ADR-0015:
                     the application creates the namespace; a restore never
                     lands on top of rows that are already there)
  -h, --help         this text

Everything else is the config: `storage.data-dir` (the database's directory,
`$HOME` allowed), the `embedding` model directory, the `compression` numbers and
the `logs` ceiling.";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut config: Option<PathBuf> = None;
    // `DEFAULT_ADDR` is this file's own literal; a broken literal is a build
    // defect, so the parse falls back to the same loopback default instead of
    // panicking at boot.
    let mut addr: SocketAddr = DEFAULT_ADDR
        .parse()
        .unwrap_or(SocketAddr::from(([127, 0, 0, 1], 7760)));

    // The restore's two shapes (ADR-0015): a target, and optionally the mapping
    // that says which tables go where. No mapping = full restore.
    let mut restore: Option<String> = None;
    let mut mapping: std::collections::BTreeMap<String, u16> = std::collections::BTreeMap::new();

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                let value = args
                    .next()
                    .ok_or_else(|| format!("--config needs a path\n{USAGE}"))?;
                config = Some(PathBuf::from(value));
            }
            "--restore" => {
                let value = args
                    .next()
                    .ok_or_else(|| format!("--restore needs a lake target\n{USAGE}"))?;
                restore = Some(value);
            }
            "--map" => {
                let value = args
                    .next()
                    .ok_or_else(|| format!("--map needs <table>=<ns>\n{USAGE}"))?;
                let (table, ns) = value
                    .split_once('=')
                    .ok_or_else(|| format!("--map wants <table>=<ns>, not `{value}`"))?;
                let ns: u16 = ns
                    .parse()
                    .map_err(|_| format!("`{ns}` is not a namespace number"))?;
                mapping.insert(table.to_string(), ns);
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

    // The restore is an OPERATOR action on a stopped service, not a door: the store
    // has one writer, so it runs here, lands the lake, and exits before any door
    // exists (ADR-0015).
    if let Some(target) = restore {
        let lake = okm_lake::store::open(&target, None)?;
        let mut stores = mem_core::service::open_service(&data_dir)?;
        let mapping = (!mapping.is_empty()).then_some(&mapping);
        let landed = mem_core::restore::land(&mut stores, &*lake, mapping)?;
        for entry in &landed {
            println!(
                "restored {:<14} ns {:<5} {} row(s)",
                entry.table, entry.ns, entry.rows
            );
        }
        println!("  from {}", target);
        println!("  into {}", data_dir.display());
        return Ok(());
    }

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
