//! minicardstat.sock: one socket for two jobs.
//!
//!  1. Status: on connect you get one line with the current state, and a new line
//!     every time the set of minimized apps changes.
//!         none\n                 -> nothing is minimized
//!         kitty,firefox\n        -> comma separated app_ids of minimized windows
//!
//!  2. Commands: write a line to the same connection.
//!         maximize <app_id>\n    -> restores every minimized card of that app
//!     Replies always start with '!' so they can't be mixed up with state lines:
//!         !ok <count>\n   or   !err <reason>\n
//!
//! CLI:  cos-ssd -C            list minimized apps (one per line)
//!       cos-ssd -m kitty      maximize kitty

use gtk4::glib;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

pub fn socket_path() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(dir).join("minicardstat.sock")
    } else {
        PathBuf::from("/tmp/minicardstat.sock")
    }
}

fn encode(apps: &[String]) -> Vec<u8> {
    if apps.is_empty() {
        b"none\n".to_vec()
    } else {
        let clean: Vec<String> = apps
            .iter()
            .map(|a| a.replace([',', '\n', '!'], "_"))
            .collect();
        format!("{}\n", clean.join(",")).into_bytes()
    }
}

type Client = Arc<Mutex<UnixStream>>;

/// A command from a socket client, executed on the GTK thread.
struct Cmd {
    app: String,
    reply: mpsc::Sender<String>,
}

#[derive(Clone)]
pub struct MiniCardBus {
    apps: Arc<Mutex<Vec<String>>>,
    clients: Arc<Mutex<Vec<Client>>>,
    cmd_tx: Arc<Mutex<mpsc::Sender<Cmd>>>,
}

impl MiniCardBus {
    /// Must run on the GTK main thread (it installs a glib poller for commands).
    fn spawn() -> Self {
        let path = socket_path();
        let _ = std::fs::remove_file(&path);

        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();

        let bus = MiniCardBus {
            apps: Arc::new(Mutex::new(Vec::new())),
            clients: Arc::new(Mutex::new(Vec::new())),
            cmd_tx: Arc::new(Mutex::new(cmd_tx)),
        };

        // GTK-side: run commands coming from socket clients.
        glib::timeout_add_local(Duration::from_millis(100), move || {
            while let Ok(cmd) = cmd_rx.try_recv() {
                let n = crate::card::maximize_app(&cmd.app);
                let msg = if n > 0 {
                    format!("!ok {n}\n")
                } else {
                    format!("!err no minimized app '{}'\n", cmd.app)
                };
                let _ = cmd.reply.send(msg);
            }
            glib::ControlFlow::Continue
        });

        match UnixListener::bind(&path) {
            Ok(listener) => {
                let bus_bg = bus.clone();
                thread::spawn(move || {
                    for conn in listener.incoming() {
                        let Ok(stream) = conn else { continue };
                        let Ok(reader) = stream.try_clone() else { continue };
                        // a stuck client must never block the bus
                        let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
                        let client: Client = Arc::new(Mutex::new(stream));

                        {
                            // snapshot + register under the client lock so a
                            // concurrent set() can't slip in between
                            let mut clients = bus_bg.clients.lock().unwrap();
                            let snapshot = bus_bg.apps.lock().unwrap().clone();
                            if client
                                .lock()
                                .unwrap()
                                .write_all(&encode(&snapshot))
                                .is_err()
                            {
                                continue;
                            }
                            clients.push(client.clone());
                        }

                        let bus_c = bus_bg.clone();
                        thread::spawn(move || serve_commands(bus_c, reader, client));
                    }
                });
            }
            Err(e) => {
                eprintln!("[minicardStat] couldn't bind {:?}: {e}", path);
            }
        }

        bus
    }

    #[allow(dead_code)]
    pub fn get(&self) -> Vec<String> {
        self.apps.lock().unwrap().clone()
    }

    pub fn set(&self, apps: Vec<String>) {
        let msg = encode(&apps);
        *self.apps.lock().unwrap() = apps;

        let mut clients = self.clients.lock().unwrap();
        clients.retain(|c| c.lock().unwrap().write_all(&msg).is_ok());
    }

    fn run_cmd(&self, app: &str) -> String {
        let (tx, rx) = mpsc::channel::<String>();
        let sent = self.cmd_tx.lock().unwrap().send(Cmd {
            app: app.to_string(),
            reply: tx,
        });
        if sent.is_err() {
            return "!err shell is shutting down\n".to_string();
        }
        rx.recv_timeout(Duration::from_secs(2))
            .unwrap_or_else(|_| "!err timed out\n".to_string())
    }
}

/// Reads commands from one client until it disconnects.
fn serve_commands(bus: MiniCardBus, reader: UnixStream, client: Client) {
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let reply = match line.trim().split_once(' ') {
            Some(("maximize", app)) if !app.trim().is_empty() => bus.run_cmd(app.trim()),
            _ => "!err unknown command\n".to_string(),
        };
        if client.lock().unwrap().write_all(reply.as_bytes()).is_err() {
            break;
        }
    }
}

static BUS: OnceLock<MiniCardBus> = OnceLock::new();

/// Binds the socket (once). Call on the GTK thread at startup so queries work
/// even when no card exists yet.
pub fn init() -> &'static MiniCardBus {
    BUS.get_or_init(MiniCardBus::spawn)
}

/// Called by card.rs whenever the set of minimized apps changes.
pub fn publish(apps: Vec<String>) {
    init().set(apps);
}

// ---------------------------------------------------------------------------
// client side (used by the CLI and by other modules in the capsule shell binary)
// ---------------------------------------------------------------------------

fn connect() -> Option<UnixStream> {
    match UnixStream::connect(socket_path()) {
        Ok(s) => {
            let _ = s.set_read_timeout(Some(Duration::from_secs(3)));
            Some(s)
        }
        Err(e) => {
            eprintln!("[minicardStat] couldn't reach a running instance: {e}");
            None
        }
    }
}

fn parse_state(line: &str) -> Vec<String> {
    let l = line.trim();
    if l.is_empty() || l == "none" {
        Vec::new()
    } else {
        l.split(',').map(|s| s.to_string()).collect()
    }
}

// code based query for modules inside the capsule shell binary
pub fn cbq_minimized() -> Vec<String> {
    let Some(stream) = connect() else { return Vec::new(); };
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) | Err(_) => {
            eprintln!("[minicardStat] no response from running instance");
            Vec::new()
        }
        Ok(_) => parse_state(&line),
    }
}

#[allow(dead_code)]
pub fn cbq_is_minimized(app_id: &str) -> bool {
    cbq_minimized()
        .iter()
        .any(|a| a.eq_ignore_ascii_case(app_id))
}

/// `cos-ssd -C`: print the minimized apps, one per line.
pub fn nlq() -> i32 {
    let Some(stream) = connect() else { return 1; };
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) | Err(_) => {
            eprintln!("[minicardStat] no response from running instance");
            1
        }
        Ok(_) => {
            for app in parse_state(&line) {
                println!("{app}");
            }
            0
        }
    }
}

/// `cos-ssd -m <app>`: maximize every minimized card of that app.
pub fn maximize(app: &str) -> i32 {
    let Some(mut stream) = connect() else { return 1; };
    let Ok(read_half) = stream.try_clone() else { return 1; };
    let mut reader = BufReader::new(read_half);

    // the server always greets with the current state; skip it
    let mut line = String::new();
    if matches!(reader.read_line(&mut line), Ok(0) | Err(_)) {
        eprintln!("[minicardStat] no response from running instance");
        return 1;
    }

    let app = app.replace('\n', " ");
    if stream
        .write_all(format!("maximize {app}\n").as_bytes())
        .is_err()
    {
        eprintln!("[minicardStat] couldn't send command");
        return 1;
    }

    // skip any state broadcasts until our reply (always starts with '!')
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => {
                eprintln!("[minicardStat] no reply from running instance");
                return 1;
            }
            Ok(_) => {
                let l = line.trim();
                if let Some(rest) = l.strip_prefix("!ok") {
                    println!("maximized {app}{}", rest);
                    return 0;
                }
                if let Some(rest) = l.strip_prefix("!err") {
                    eprintln!("{}", rest.trim());
                    return 1;
                }
            }
        }
    }
}