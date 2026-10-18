//! Escaneo paralelo: encuentra carpetas objetivo (node_modules) y calcula su tamaño.

use rayon::prelude::*;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::time::SystemTime;

pub struct ScanOptions {
    pub targets: Vec<OsString>,
    pub exclude: Vec<OsString>,
    pub exclude_hidden: bool,
    pub cancel: Arc<AtomicBool>,
}

impl ScanOptions {
    fn is_target(&self, name: &OsStr) -> bool {
        self.targets.iter().any(|t| t == name)
    }

    fn is_excluded(&self, name: &OsStr) -> bool {
        (self.exclude_hidden && name.as_encoded_bytes().first() == Some(&b'.'))
            || self.exclude.iter().any(|e| e == name)
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

pub struct Found {
    pub id: usize,
    pub path: PathBuf,
    /// Última modificación del proyecto (hijos directos del padre, sin contar los objetivos).
    pub modified: Option<SystemTime>,
    pub risky: bool,
}

pub enum ScanEvent {
    Found(Found),
    Sized { id: usize, bytes: u64 },
    Done,
}

/// Recorre `root` en paralelo. Envía `Found` en cuanto aparece un objetivo y `Sized`
/// cuando termina de medirlo; `Done` al final. `Found(id)` siempre llega antes que `Sized(id)`.
pub fn scan(root: &Path, opts: &ScanOptions, tx: Sender<ScanEvent>) {
    let next_id = AtomicUsize::new(0);
    let tx = &tx;
    let next_id = &next_id;
    rayon::scope(|s| walk(s, root.to_path_buf(), root, opts, tx, next_id));
    let _ = tx.send(ScanEvent::Done);
}

fn walk<'s>(
    s: &rayon::Scope<'s>,
    dir: PathBuf,
    root: &'s Path,
    opts: &'s ScanOptions,
    tx: &'s Sender<ScanEvent>,
    next_id: &'s AtomicUsize,
) {
    if opts.cancelled() {
        return;
    }
    let Ok(rd) = fs::read_dir(&dir) else { return };
    let mut project_mtime: Option<Option<SystemTime>> = None;

    for entry in rd.flatten() {
        // `is_dir()` es falso para symlinks y junctions: nunca los seguimos.
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let path = entry.path();

        if opts.is_target(&name) {
            let modified = *project_mtime.get_or_insert_with(|| newest_mtime(&dir, opts));
            let id = next_id.fetch_add(1, Ordering::Relaxed);
            let found = Found {
                id,
                risky: is_risky(path.strip_prefix(root).unwrap_or(&path)),
                modified,
                path: path.clone(),
            };
            if tx.send(ScanEvent::Found(found)).is_err() {
                opts.cancel.store(true, Ordering::Relaxed);
                return;
            }
            s.spawn(move |_| {
                let bytes = dir_size(&path, &opts.cancel);
                let _ = tx.send(ScanEvent::Sized { id, bytes });
            });
        } else if !opts.is_excluded(&name) {
            s.spawn(move |s| walk(s, path, root, opts, tx, next_id));
        }
    }
}

/// Fecha más reciente entre los hijos directos de `dir`, ignorando las carpetas objetivo.
fn newest_mtime(dir: &Path, opts: &ScanOptions) -> Option<SystemTime> {
    let rd = fs::read_dir(dir).ok()?;
    rd.flatten()
        .filter(|e| !opts.is_target(&e.file_name()))
        .filter_map(|e| e.metadata().ok()?.modified().ok())
        .max()
        .or_else(|| fs::metadata(dir).ok()?.modified().ok())
}

/// Tamaño aparente de un árbol, recorriendo subcarpetas en paralelo.
/// En Windows `DirEntry::metadata` sale gratis de FindNextFile (sin syscalls extra).
pub fn dir_size(dir: &Path, cancel: &AtomicBool) -> u64 {
    if cancel.load(Ordering::Relaxed) {
        return 0;
    }
    let Ok(rd) = fs::read_dir(dir) else { return 0 };
    let mut bytes = 0;
    let mut subdirs = Vec::new();
    for entry in rd.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            subdirs.push(entry.path());
        } else if let Ok(md) = entry.metadata() {
            bytes += md.len();
        }
    }
    bytes
        + match subdirs.as_slice() {
            [] => 0,
            [one] => dir_size(one, cancel),
            many => many.par_iter().map(|d| dir_size(d, cancel)).sum(),
        }
}

/// Rutas donde un node_modules probablemente pertenece a una app o herramienta instalada.
/// Se evalúa relativo a la raíz: si el usuario escanea dentro de AppData, lo hace a propósito.
fn is_risky(path: &Path) -> bool {
    const RISKY: &[&str] = &[
        "/appdata/",
        "/program files",
        "/.vscode",
        "/.cursor",
        "/.nvm/",
        "/.npm/",
        "/.cache/",
        "/.local/share/",
        "/.config/",
        "/applications/",
        "/library/",
        "/usr/lib/",
        "/usr/local/lib/",
        "/opt/",
    ];
    let p = format!(
        "/{}",
        path.to_string_lossy().replace('\\', "/").to_lowercase()
    );
    RISKY.iter().any(|r| p.contains(r))
}
