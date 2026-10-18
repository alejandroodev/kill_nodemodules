//! Borrado paralelo de árboles de directorios.

use rayon::prelude::*;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub struct Outcome {
    /// Bytes de archivos efectivamente borrados.
    pub bytes: u64,
    /// Primer error encontrado (el borrado continúa con lo demás).
    pub error: Option<io::Error>,
}

/// Borra `path` por completo. Si es un symlink/junction, solo quita el enlace.
pub fn delete_dir(path: &Path) -> Outcome {
    match fs::symlink_metadata(path) {
        Ok(md) if md.file_type().is_symlink() => Outcome {
            bytes: 0,
            error: remove_link(path).err(),
        },
        Ok(_) => remove_tree(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Outcome {
            bytes: 0,
            error: None,
        },
        Err(e) => Outcome {
            bytes: 0,
            error: Some(e),
        },
    }
}

fn remove_tree(dir: &Path) -> Outcome {
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => {
            return Outcome {
                bytes: 0,
                error: Some(e),
            };
        }
    };

    let mut first_err: Option<io::Error> = None;
    let mut subdirs: Vec<PathBuf> = Vec::new();
    let mut files: Vec<(PathBuf, bool, u64)> = Vec::new();
    for entry in rd {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                first_err.get_or_insert(e);
                continue;
            }
        };
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(e) => {
                first_err.get_or_insert(e);
                continue;
            }
        };
        if ft.is_dir() {
            subdirs.push(entry.path());
        } else {
            let link = ft.is_symlink();
            let len = if link {
                0
            } else {
                entry.metadata().map(|m| m.len()).unwrap_or(0)
            };
            files.push((entry.path(), link, len));
        }
    }

    let merge = |a: Outcome, b: Outcome| Outcome {
        bytes: a.bytes + b.bytes,
        error: a.error.or(b.error),
    };
    let empty = || Outcome {
        bytes: 0,
        error: None,
    };
    let (sub, own) = rayon::join(
        || {
            subdirs
                .par_iter()
                .map(|d| remove_tree(d))
                .reduce(empty, merge)
        },
        || {
            files
                .par_iter()
                .with_min_len(16)
                .map(|(p, link, len)| {
                    match if *link {
                        remove_link(p)
                    } else {
                        remove_file(p)
                    } {
                        Ok(()) => Outcome {
                            bytes: *len,
                            error: None,
                        },
                        Err(e) => Outcome {
                            bytes: 0,
                            error: Some(e),
                        },
                    }
                })
                .reduce(empty, merge)
        },
    );

    let mut out = merge(own, sub);
    out.error = first_err.or(out.error);
    if let Err(e) = remove_empty_dir(dir) {
        out.error.get_or_insert(e);
    }
    out
}

fn remove_file(p: &Path) -> io::Result<()> {
    retry_readonly(p, || fs::remove_file(p))
}

/// Symlinks a directorios y junctions en Windows se borran con RemoveDirectory.
fn remove_link(p: &Path) -> io::Result<()> {
    retry_readonly(p, || match fs::remove_file(p) {
        Err(_) if cfg!(windows) => fs::remove_dir(p),
        r => r,
    })
}

fn remove_empty_dir(dir: &Path) -> io::Result<()> {
    let mut attempt = 0;
    loop {
        match retry_readonly(dir, || fs::remove_dir(dir)) {
            // Antivirus / indexador pueden retener handles un instante en Windows.
            Err(e) if e.kind() == io::ErrorKind::DirectoryNotEmpty && attempt < 5 => {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(10 * attempt));
            }
            Err(e) if e.kind() == io::ErrorKind::DirectoryNotEmpty => {
                return fs::remove_dir_all(dir);
            }
            r => return r,
        }
    }
}

fn retry_readonly(p: &Path, op: impl Fn() -> io::Result<()>) -> io::Result<()> {
    match op() {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied && clear_readonly(p) => op(),
        Err(e) => Err(e),
    }
}

/// Quita el atributo de solo lectura (Windows). Devuelve true si cambió algo.
#[cfg(windows)]
fn clear_readonly(p: &Path) -> bool {
    let Ok(md) = fs::symlink_metadata(p) else {
        return false;
    };
    let mut perms = md.permissions();
    if !perms.readonly() {
        return false;
    }
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    fs::set_permissions(p, perms).is_ok()
}

#[cfg(not(windows))]
fn clear_readonly(_: &Path) -> bool {
    false
}
