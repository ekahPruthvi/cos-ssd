use gtk4::{gdk::Display, gio::File, prelude::*, Application, CssProvider};
use self_update::cargo_crate_version;
use std::{env, process::exit};

mod ssd;
mod card;
mod minicardstat;

const APP_ID: &str = "ekah.scu.cosssd";
const BIN_PATH: &str = "/usr/bin/cos-ssd";
const REPO_OWNER: &str = "ekahPruthvi";
const REPO_NAME: &str = "cos-ssd";

fn load_css() {
    let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let css_path = format!("{}/.config/capsule/dark.css", home);

    let css = CssProvider::new();
    css.load_from_file(&File::for_path(&css_path));

    match Display::default() {
        Some(display) => gtk4::style_context_add_provider_for_display(
            &display,
            &css,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        ),
        None => eprintln!("[cos-ssd] no default display, CSS not applied"),
    }
}

fn on_activate(app: &Application) {
    std::mem::forget(app.hold());
    load_css();
    ssd::spawn_shelly_side_decorations(app);
}

struct UpdateOutcome {
    updated: bool,
    version: String,
}

fn ensure_writable(path: &str) -> Result<(), String> {
    use std::ffi::CString;

    let c_path = CString::new(path).map_err(|e| e.to_string())?;
    let path_obj = std::path::Path::new(path);

    if path_obj.exists() {
        let writable = unsafe { libc::access(c_path.as_ptr(), libc::W_OK) == 0 };
        if !writable {
            return Err(format!(
                "no write permission for '{}'. Try: sudo cos-ssd --update",
                path
            ));
        }
    }

    if let Some(parent) = path_obj.parent() {
        let c_parent =
            CString::new(parent.to_string_lossy().to_string()).map_err(|e| e.to_string())?;
        let dir_writable = unsafe { libc::access(c_parent.as_ptr(), libc::W_OK) == 0 };
        if !dir_writable {
            return Err(format!(
                "no write permission for directory '{}'. Try: sudo cos-ssd --update",
                parent.display()
            ));
        }
    }

    Ok(())
}

fn check_for_updates() -> Result<UpdateOutcome, Box<dyn std::error::Error>> {
    let status = self_update::backends::github::Update::configure()
        .repo_owner(REPO_OWNER)
        .repo_name(REPO_NAME)
        .bin_name("cos-ssd")
        .target("x86_64")
        .bin_install_path(BIN_PATH)
        .show_download_progress(true)
        .no_confirm(true)
        .current_version(cargo_crate_version!())
        .build()?
        .update()?;

    Ok(UpdateOutcome {
        updated: status.is_updated(),
        version: status.version().to_string(),
    })
}

fn run_update() -> i32 {
    println!("Checking for updates...");

    if let Err(msg) = ensure_writable(BIN_PATH) {
        eprintln!("Cannot update cos-ssd: {}", msg);
        return 1;
    }

    match check_for_updates() {
        Ok(o) if o.updated => {
            println!("cos-ssd updated to version {}", o.version);
            0
        }
        Ok(o) => {
            println!("cos-ssd is already up to date ({})", o.version);
            0
        }
        Err(e) => {
            eprintln!("Update failed: {}", e);
            1
        }
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();

    match args.get(1).map(String::as_str) {
        Some("--version") | Some("-V") => {
            println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
            exit(0);
        }
        Some("--update") | Some("-U") => exit(run_update()),
        Some("--cards") | Some("-C") => exit(minicardstat::nlq()),
        Some("--maximize") | Some("-m") | Some("-M") => match args.get(2) {
            Some(app_id) => exit(minicardstat::maximize(app_id)),
            None => {
                eprintln!("usage: cos-ssd -m <window_id|app_id>   (e.g. cos-ssd -m 41, cos-ssd -m kitty)");
                exit(2);
            }
        },
        Some("--help") | Some("-H") => {
            println!("cos-ssd {}", env!("CARGO_PKG_VERSION"));
            println!("Display Server-side decorations for niri (cynageOS)\n");
            println!("  -V, --version        print version");
            println!("  -U, --update         update from github.com/{}/{}", REPO_OWNER, REPO_NAME);
            println!("  -C, --cards          list minimized windows (cards): id<TAB>app_id per line");
            println!("  -m, --maximize ID|APP  maximize a minimized window by id, or every one of an app, e.g. -m 41 / -m kitty");
            println!("  -H, --help           show this help");
            exit(0);
        }
        Some(other) if other.starts_with('-') => {
            eprintln!("cos-ssd: unknown option '{other}' (try --help)");
            exit(2);
        }
        _ => {}
    }

    let app = Application::new(Some(APP_ID), Default::default());
    app.connect_activate(on_activate);
    app.run_with_args::<&str>(&[]);
}