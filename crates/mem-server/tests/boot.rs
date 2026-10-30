//! The binary's boot contract: ONE config file, found beside the binary unless
//! `--config` names another, and everything the process serves comes from it —
//! the database's directory (`$HOME` expanded), the embedding engine, the policy
//! numbers.
//!
//! These tests start the REAL process (`CARGO_BIN_EXE_krystallizer`) instead of
//! building a router in-process: the thing under test is the boot path — argument
//! parsing, the file's default location, and the config reaching the stores. The
//! model artifact is ~96 MB and deliberately NOT in the repo, so every test here
//! skips (printing why) on a machine without it.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

const CONFIG_FILE: &str = "krystallizer.kdl";

/// A spawned process that is killed when the test ends, whether it passed or
/// panicked — a leaked server would hold the database's lock and the port.
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The boot lines, once the door is listening: the bound address plus the two
/// paths the process reports it is serving. Holds the process alive — the caller
/// drops it (killing the child) when the assertions are done.
struct Booted {
    addr: SocketAddr,
    config: String,
    data: String,
    _server: Server,
}

fn boot(mut server: Server) -> Booted {
    let mut lines = BufReader::new(server.0.stdout.take().expect("stdout is piped"));
    let mut line = String::new();
    let addr = loop {
        line.clear();
        match lines.read_line(&mut line) {
            Ok(0) => panic!(
                "the process exited before it listened: {}",
                drain_stderr(&mut server.0)
            ),
            Ok(_) => {
                if let Some(rest) = line
                    .trim_end()
                    .strip_prefix("krystallizer listening on http://")
                {
                    break rest.parse::<SocketAddr>().expect("the address parses");
                }
            }
            Err(e) => panic!("cannot read the boot output: {e}"),
        }
    };
    let config = field(
        &next_line(&mut lines, &mut server.0, "the config path"),
        "config",
    );
    let data = field(
        &next_line(&mut lines, &mut server.0, "the data path"),
        "data",
    );
    Booted {
        addr,
        config,
        data,
        _server: server,
    }
}

/// One more boot line, or the process's own words if it stopped here.
fn next_line(lines: &mut impl BufRead, child: &mut Child, what: &str) -> String {
    let mut line = String::new();
    if lines.read_line(&mut line).unwrap_or(0) == 0 {
        panic!(
            "the process stopped before reporting {what}: {}",
            drain_stderr(child)
        );
    }
    line
}

fn field(line: &str, name: &str) -> String {
    line.trim()
        .strip_prefix(name)
        .unwrap_or_else(|| panic!("the boot line does not start with `{name}`: {line}"))
        .trim()
        .to_string()
}

fn drain_stderr(child: &mut Child) -> String {
    let mut text = String::new();
    if let Some(mut stderr) = child.stderr.take() {
        let _ = stderr.read_to_string(&mut text);
    }
    text
}

/// One plain HTTP GET over std, answered whole: the door's liveness is what is
/// being checked, not a client's ergonomics.
fn http_get(addr: SocketAddr, path: &str) -> String {
    let mut stream = std::net::TcpStream::connect(addr).expect("the door accepts a connection");
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    )
    .expect("the request is written");
    let mut answer = String::new();
    stream
        .read_to_string(&mut answer)
        .expect("the answer is read");
    answer
}

/// The repo config's own model directory, or None when this machine has no
/// artifact (the convention the embedding tests follow).
fn shipped_model_dir() -> Option<PathBuf> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/krystallizer.kdl");
    let cfg = mem_core::KrystallizerConfig::load(&path).ok()?;
    let dir = cfg.embedding.resolved_model_dir().ok()?;
    if !dir.join("model.safetensors").exists() {
        eprintln!("skipping: no embedding model in {}", dir.display());
        return None;
    }
    Some(dir)
}

/// A config with this test's own data directory and the machine's model.
fn config_text(data_dir: &str, model_dir: &Path) -> String {
    format!(
        r#"storage {{
    data-dir "{data}"
}}
embedding {{
    mode "local"
    model-dir "{model}"
    dimensions 512
    metric "cosine"
}}
compression {{
    context-bytes 131072
    budget-percent 50
    cache-ttl-ms 300000
    floor-bytes 2048
}}
logs {{
    max-rows 10000
}}
"#,
        data = data_dir,
        model = model_dir.display(),
    )
}

/// Stage a deployment: the binary and, optionally, the config beside it. This is
/// the layout the default lookup assumes, so it is what a test has to reproduce.
///
/// The copy is a plain read + write, not `fs::copy`: that one uses
/// `copy_file_range` on Linux, and exec'ing the result immediately after can fail
/// with `ETXTBSY` while the kernel still holds the writer's reference.
fn deploy(root: &Path, with_config: Option<&str>) -> PathBuf {
    let bin = root.join("krystallizer");
    let image = std::fs::read(env!("CARGO_BIN_EXE_krystallizer")).expect("the binary is read");
    std::fs::write(&bin, image).expect("the binary is written beside the config");
    // A written file gets 0644; the staged copy has to be executable.
    std::fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .expect("the staged binary is executable");
    if let Some(text) = with_config {
        std::fs::write(root.join(CONFIG_FILE), text).expect("the config is written");
    }
    bin
}

/// Start the staged binary. `ETXTBSY` is retried for the same reason the copy is
/// a plain write: the kernel can still refuse `exec` on a file written a moment
/// ago, and a boot test must not be a coin flip.
fn start(command: &mut Command) -> Child {
    let mut attempts = 0;
    loop {
        match command.spawn() {
            Ok(child) => return child,
            Err(e) if attempts < 50 && e.raw_os_error() == Some(26) => {
                attempts += 1;
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => panic!("the binary does not start: {e}"),
        }
    }
}

/// The requirement and the default together: no `--config`, so the file is the
/// one beside the binary, and the database lands in the directory IT names.
#[test]
fn the_config_beside_the_binary_is_the_one_read() {
    let Some(model) = shipped_model_dir() else {
        return;
    };
    let root = tempfile::tempdir().expect("a temp dir");
    let data = root.path().join("data");
    let text = config_text(&data.display().to_string(), &model);
    let bin = deploy(root.path(), Some(&text));

    let child = start(
        Command::new(&bin)
            .args(["--addr", "127.0.0.1:0"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    );
    let booted = boot(Server(child));

    assert_eq!(
        booted.config,
        root.path().join(CONFIG_FILE).display().to_string(),
        "the config found is the one beside the binary"
    );
    assert_eq!(
        booted.data,
        data.display().to_string(),
        "the data directory is the config's storage.data-dir"
    );
    assert!(
        data.is_dir(),
        "the configured database directory is created"
    );
    let answer = http_get(booted.addr, "/graph/0");
    assert!(
        answer.starts_with("HTTP/1.1 200"),
        "the door answers: {answer}"
    );
    assert!(
        answer.contains("\"nodes\":[]"),
        "an empty scope is an empty graph: {answer}"
    );
}

/// `--config` wins over the file beside the binary, and a `$HOME` path in it is
/// expanded against the CHILD's environment — the placeholder is the process's,
/// not the file's.
#[test]
fn an_explicit_config_wins_and_expands_home() {
    let Some(model) = shipped_model_dir() else {
        return;
    };
    let root = tempfile::tempdir().expect("a temp dir");
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).expect("a home for the child");
    // No config beside the binary: the only one is the one named.
    let bin = deploy(root.path(), None);
    let elsewhere = root.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).expect("a directory for the config");
    let config = elsewhere.join("custom.kdl");
    std::fs::write(&config, config_text("$HOME/krystallizer-data", &model))
        .expect("the config is written");

    let child = start(
        Command::new(&bin)
            .args([
                "--config",
                &config.display().to_string(),
                "--addr",
                "127.0.0.1:0",
            ])
            .env("HOME", &home)
            .env_remove(mem_core::config::MODEL_DIR_ENV)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    );
    let booted = boot(Server(child));

    assert_eq!(booted.config, config.display().to_string());
    let expected = home.join("krystallizer-data");
    assert_eq!(
        booted.data,
        expected.display().to_string(),
        "$HOME is expanded against the child's environment"
    );
    assert!(expected.is_dir(), "the expanded directory is created");
    let answer = http_get(booted.addr, "/graph/0");
    assert!(
        answer.starts_with("HTTP/1.1 200"),
        "the door answers: {answer}"
    );
}

/// No config anywhere and no `--config`: the boot fails and says what it was
/// looking for. A service that invented a directory here would be serving a
/// second, invisible database.
#[test]
fn a_missing_config_is_a_boot_failure_that_names_the_file() {
    let root = tempfile::tempdir().expect("a temp dir");
    let bin = deploy(root.path(), None);
    let output = Command::new(&bin)
        .args(["--addr", "127.0.0.1:0"])
        .output()
        .expect("the binary runs");
    assert!(
        !output.status.success(),
        "the process does not serve without a config"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(CONFIG_FILE),
        "the failure names the file it looked for: {stderr}"
    );
}
