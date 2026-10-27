//! Feedstr for desktop Linux.

use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::sync::{Arc, OnceLock};

use eframe::egui;
use feedstr_core::{App, FileKind, Platform};
use futures::future::BoxFuture;

const APP_ID: &str = "top.dns7.feedstr.Feedstr";

/// Prints the app's warnings to stderr.
struct Stderr;

impl log::Log for Stderr {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Warn
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            eprintln!("{}: {}", record.level(), record.args());
        }
    }

    fn flush(&self) {}
}

fn main() -> anyhow::Result<()> {
    let _ = log::set_logger(&Stderr).map(|()| log::set_max_level(log::LevelFilter::Warn));
    let dir = dirs::data_dir().ok_or_else(|| anyhow::anyhow!("no data directory"))?.join("feedstr");
    std::fs::create_dir_all(&dir)?;
    // The database holds the Sync Key, so only this user may read it.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    let db_path = dir.join("feedstr.db");

    // On Wayland the desktop takes the icon from the launcher entry (by app ID); X11 needs it here.
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../icon.png"))?;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Feedstr")
            .with_app_id(APP_ID)
            .with_icon(icon)
            .with_inner_size([1200.0, 780.0])
            .with_min_inner_size([760.0, 480.0]),
        ..Default::default()
    };
    let app = move |cc: &eframe::CreationContext| App::new(cc, db_path, Arc::new(Dialogs));
    eframe::run_native("Feedstr", options, Box::new(|cc| Ok(Box::new(app(cc)?)))).map_err(|e| anyhow::anyhow!("{e}"))
}

/// Files are picked with the desktop's file dialogs, and Nostr links opened
/// with the desktop's handler for them.
struct Dialogs;

impl Platform for Dialogs {
    fn open_file(&self, kind: FileKind) -> BoxFuture<'static, anyhow::Result<Option<Vec<u8>>>> {
        let dialog = rfd::AsyncFileDialog::new().set_title("Import").add_filter(kind.label(), kind.extensions());
        Box::pin(async move {
            let Some(file) = dialog.pick_file().await else { return Ok(None) };
            // Not read at all if it's too big, so a wrong pick can't fill the memory.
            if std::fs::metadata(file.path())?.len() > feedstr_core::MAX_IMPORT_BYTES as u64 {
                anyhow::bail!("That file is too big to import (over {} MB)", feedstr_core::MAX_IMPORT_BYTES >> 20);
            }
            Ok(Some(file.read().await))
        })
    }

    fn save_file(
        &self,
        kind: FileKind,
        name: String,
        content: Vec<u8>,
    ) -> BoxFuture<'static, anyhow::Result<Option<String>>> {
        let dialog = rfd::AsyncFileDialog::new().set_file_name(name).add_filter(kind.label(), kind.extensions());
        Box::pin(async move {
            let Some(file) = dialog.save_file().await else { return Ok(None) };
            file.write(&content).await?;
            Ok(Some(file.path().display().to_string()))
        })
    }

    fn opens_nostr_links(&self) -> bool {
        static INSTALLED: OnceLock<bool> = OnceLock::new();
        *INSTALLED.get_or_init(|| {
            let handler = Command::new("xdg-mime").args(["query", "default", "x-scheme-handler/nostr"]).output();
            handler.is_ok_and(|out| out.status.success() && !out.stdout.trim_ascii().is_empty())
        })
    }

    fn open_nostr_link(&self, link: &str) -> anyhow::Result<()> {
        // No shell, and core only makes links of "nostr:" and bech32 characters.
        anyhow::ensure!(link.starts_with("nostr:"), "not a Nostr link");
        let mut opener = Command::new("xdg-open").arg(link).spawn()?;
        std::thread::spawn(move || opener.wait());
        Ok(())
    }
}
