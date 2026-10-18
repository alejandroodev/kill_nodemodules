mod delete;
mod scan;
mod tui;
mod util;

use clap::{Parser, ValueEnum};
use rayon::prelude::*;
use scan::{ScanEvent, ScanOptions};
use std::collections::HashMap;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::time::{Instant, SystemTime};
use util::{format_age, format_size, json_escape};

/// Busca y borra carpetas node_modules a toda velocidad.
///
/// Sin flags abre una interfaz interactiva: muévete con ↑/↓ y pulsa Espacio para borrar.
#[derive(Parser)]
#[command(name = "nmkill", version)]
struct Cli {
    /// Carpeta donde buscar
    #[arg(default_value = ".")]
    dir: PathBuf,

    /// Nombres de carpeta a buscar (separados por comas)
    #[arg(short, long, value_delimiter = ',', default_value = "node_modules")]
    target: Vec<String>,

    /// Nombres de carpeta que no se recorren (separados por comas)
    #[arg(short = 'E', long, value_delimiter = ',', default_value = ".git")]
    exclude: Vec<String>,

    /// No entrar en carpetas ocultas (que empiezan por '.')
    #[arg(long)]
    exclude_hidden: bool,

    /// Orden de los resultados
    #[arg(short, long, value_enum, default_value_t = SortBy::Size)]
    sort: SortBy,

    /// Solo proyectos sin cambios desde hace al menos N días
    #[arg(long, value_name = "DÍAS")]
    older_than: Option<u64>,

    /// Hilos de trabajo (por defecto: 2 × núcleos)
    #[arg(short = 'j', long)]
    threads: Option<usize>,

    /// Listar resultados y salir (sin interfaz)
    #[arg(short, long)]
    list: bool,

    /// Salida en JSON (con --list o --delete-all)
    #[arg(long)]
    json: bool,

    /// Borrar todo lo encontrado (sin interfaz)
    #[arg(short = 'D', long)]
    delete_all: bool,

    /// No pedir confirmación con --delete-all
    #[arg(short, long)]
    yes: bool,

    /// Con --delete-all: mostrar qué se borraría sin borrar nada
    #[arg(long)]
    dry_run: bool,

    /// Con --delete-all: incluir rutas sensibles (AppData, .vscode, Program Files…)
    #[arg(long)]
    include_risky: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SortBy {
    Size,
    Path,
    Age,
}

impl SortBy {
    pub fn next(self) -> Self {
        match self {
            SortBy::Size => SortBy::Age,
            SortBy::Age => SortBy::Path,
            SortBy::Path => SortBy::Size,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SortBy::Size => "tamaño",
            SortBy::Age => "antigüedad",
            SortBy::Path => "ruta",
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    let root = match std::fs::canonicalize(&cli.dir) {
        Ok(p) => simplify_path(p),
        Err(e) => {
            eprintln!("nmkill: no se puede abrir {}: {e}", cli.dir.display());
            return ExitCode::FAILURE;
        }
    };

    // El trabajo es casi todo E/S: más hilos que núcleos mantiene el disco ocupado.
    let threads = cli
        .threads
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(8, |n| n.get() * 2));
    let _ = rayon::ThreadPoolBuilder::new()
        .num_threads(threads.max(1))
        .build_global();

    let opts = ScanOptions {
        targets: cli.target.iter().map(Into::into).collect(),
        exclude: cli
            .exclude
            .iter()
            .filter(|e| !e.is_empty())
            .map(Into::into)
            .collect(),
        exclude_hidden: cli.exclude_hidden,
        cancel: Arc::new(AtomicBool::new(false)),
    };

    let result = if cli.list || cli.delete_all {
        batch(&cli, &root, opts)
    } else {
        tui::run(tui::Config {
            root,
            opts,
            sort: cli.sort,
            older_than: cli.older_than,
        })
    };

    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("nmkill: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Quita el prefijo `\\?\` que añade `canonicalize` en Windows.
fn simplify_path(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => p,
    }
}

pub struct Item {
    pub path: PathBuf,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub risky: bool,
}

fn collect(root: &Path, opts: &ScanOptions) -> Vec<Item> {
    let (tx, rx) = mpsc::channel();
    scan::scan(root, opts, tx);
    let mut items: Vec<Item> = Vec::new();
    let mut by_id = HashMap::new();
    for ev in rx {
        match ev {
            ScanEvent::Found(f) => {
                by_id.insert(f.id, items.len());
                items.push(Item {
                    path: f.path,
                    size: 0,
                    modified: f.modified,
                    risky: f.risky,
                });
            }
            ScanEvent::Sized { id, bytes } => items[by_id[&id]].size = bytes,
            ScanEvent::Done => break,
        }
    }
    items
}

type SortKey<'a> = (Option<u64>, Option<SystemTime>, &'a Path);

pub fn compare(sort: SortBy, (sa, ma, pa): SortKey, (sb, mb, pb): SortKey) -> std::cmp::Ordering {
    let by_path = || pa.cmp(pb);
    match sort {
        // Más grande primero; los que aún se están midiendo al final.
        SortBy::Size => sb.cmp(&sa).then_with(by_path),
        // Más antiguo primero; sin fecha al final.
        SortBy::Age => match (ma, mb) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
        .then_with(by_path),
        SortBy::Path => by_path(),
    }
}

fn batch(cli: &Cli, root: &Path, opts: ScanOptions) -> io::Result<ExitCode> {
    let started = Instant::now();
    if !cli.json {
        eprintln!("Buscando en {} …", root.display());
    }
    let mut items = collect(root, &opts);
    let scan_time = started.elapsed();

    if let Some(days) = cli.older_than {
        items.retain(|i| util::age_days(i.modified).is_some_and(|d| d >= days));
    }
    let skipped_risky = if cli.delete_all && !cli.include_risky {
        let before = items.len();
        items.retain(|i| !i.risky);
        before - items.len()
    } else {
        0
    };
    items.sort_by(|a, b| {
        compare(
            cli.sort,
            (Some(a.size), a.modified, &a.path),
            (Some(b.size), b.modified, &b.path),
        )
    });
    let total: u64 = items.iter().map(|i| i.size).sum();

    if cli.json && (cli.list || cli.dry_run) {
        print_json(&items, None);
        return Ok(ExitCode::SUCCESS);
    }

    if !cli.json {
        print_table(root, &items);
        eprintln!(
            "\n{} carpeta(s), {} en total · escaneo en {:.2?}",
            items.len(),
            format_size(total),
            scan_time
        );
        if skipped_risky > 0 {
            eprintln!(
                "Omitidas {skipped_risky} carpeta(s) en rutas sensibles (usa --include-risky para incluirlas)."
            );
        }
    }

    if !cli.delete_all || cli.dry_run || items.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }

    if !cli.yes {
        if !io::stdin().is_terminal() {
            eprintln!("Usa --yes para borrar sin terminal interactiva.");
            return Ok(ExitCode::FAILURE);
        }
        eprint!(
            "¿Borrar {} carpeta(s) ({})? [s/N] ",
            items.len(),
            format_size(total)
        );
        io::stderr().flush()?;
        let mut answer = String::new();
        io::stdin().lock().read_line(&mut answer)?;
        if !matches!(
            answer.trim().to_lowercase().as_str(),
            "s" | "si" | "sí" | "y" | "yes"
        ) {
            eprintln!("Cancelado.");
            return Ok(ExitCode::SUCCESS);
        }
    }

    let started = Instant::now();
    let results: Vec<(u64, Option<io::Error>)> = items
        .par_iter()
        .map(|item| {
            let out = delete::delete_dir(&item.path);
            if !cli.json {
                match &out.error {
                    None => println!("✓ {:>11}  {}", format_size(out.bytes), item.path.display()),
                    Some(e) => println!(
                        "✗ {:>11}  {}  ({e})",
                        format_size(out.bytes),
                        item.path.display()
                    ),
                }
            }
            (out.bytes, out.error)
        })
        .collect();

    let freed: u64 = results.iter().map(|r| r.0).sum();
    let failed = results.iter().filter(|r| r.1.is_some()).count();
    if cli.json {
        print_json(&items, Some(&results));
    } else {
        eprintln!(
            "\nLiberados {} en {:.2?}.",
            format_size(freed),
            started.elapsed()
        );
        if failed > 0 {
            eprintln!("{failed} carpeta(s) no se pudieron borrar por completo.");
        }
    }
    Ok(if failed > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn print_table(root: &Path, items: &[Item]) {
    let mut out = io::stdout().lock();
    for i in items {
        let rel = i.path.strip_prefix(root).unwrap_or(&i.path);
        let warn = if i.risky { "⚠ " } else { "" };
        let _ = writeln!(
            out,
            "{:>11}  {:>5}  {warn}{}",
            format_size(i.size),
            format_age(i.modified),
            rel.display()
        );
    }
}

fn print_json(items: &[Item], results: Option<&[(u64, Option<io::Error>)]>) {
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "[");
    for (n, i) in items.iter().enumerate() {
        let modified = i
            .modified
            .and_then(|m| m.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map_or("null".into(), |d| d.as_secs().to_string());
        let mut line = format!(
            "  {{\"path\": \"{}\", \"size\": {}, \"modified\": {}, \"risky\": {}",
            json_escape(&i.path.to_string_lossy()),
            i.size,
            modified,
            i.risky
        );
        if let Some((freed, err)) = results.map(|r| &r[n]) {
            let err = err.as_ref().map_or("null".into(), |e| {
                format!("\"{}\"", json_escape(&e.to_string()))
            });
            line.push_str(&format!(", \"freed\": {freed}, \"error\": {err}"));
        }
        let comma = if n + 1 < items.len() { "," } else { "" };
        let _ = writeln!(out, "{line}}}{comma}");
    }
    let _ = writeln!(out, "]");
}
