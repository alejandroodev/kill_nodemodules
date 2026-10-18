//! Interfaz interactiva al estilo npkill.

use crate::delete;
use crate::scan::{self, ScanEvent, ScanOptions};
use crate::util::{age_days, format_age, format_size, truncate_left};
use crate::{SortBy, compare};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Cell, HighlightSpacing, Paragraph, Row, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Table, TableState,
};
use ratatui::{DefaultTerminal, Frame};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant, SystemTime};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;
const SELECTED_BG: Color = Color::Indexed(237);

pub struct Config {
    pub root: PathBuf,
    pub opts: ScanOptions,
    pub sort: SortBy,
    pub older_than: Option<u64>,
}

#[derive(PartialEq)]
enum Status {
    Present,
    Deleting,
    Deleted,
    Failed(String),
}

struct Entry {
    path: PathBuf,
    display: String,
    size: Option<u64>,
    modified: Option<SystemTime>,
    risky: bool,
    marked: bool,
    status: Status,
}

impl Entry {
    fn deletable(&self) -> bool {
        matches!(self.status, Status::Present | Status::Failed(_))
    }
}

struct DelMsg {
    idx: usize,
    bytes: u64,
    error: Option<String>,
}

enum Flow {
    Continue,
    Quit,
}

pub fn run(cfg: Config) -> io::Result<ExitCode> {
    let cancel = cfg.opts.cancel.clone();
    let (scan_tx, scan_rx) = mpsc::channel();
    let (del_tx, del_rx) = mpsc::channel();
    let opts = Arc::new(cfg.opts);
    let root = cfg.root.clone();
    std::thread::spawn(move || scan::scan(&root, &opts, scan_tx));

    let mut app = App::new(cfg.root, cfg.sort, cfg.older_than, del_tx);
    let mut terminal = ratatui::init();
    let result = app.event_loop(&mut terminal, &scan_rx, &del_rx);
    ratatui::restore();
    cancel.store(true, Ordering::Relaxed);
    result?;

    // No dejamos carpetas a medio borrar al salir.
    if app.pending > 0 {
        eprintln!("Esperando a que terminen {} borrado(s)…", app.pending);
        while app.pending > 0 {
            match del_rx.recv() {
                Ok(m) => app.on_deleted(m),
                Err(_) => break,
            }
        }
    }
    if app.deleted_count > 0 {
        println!(
            "Liberados {} en {} carpeta(s).",
            format_size(app.freed),
            app.deleted_count
        );
    }
    Ok(ExitCode::SUCCESS)
}

struct App {
    root: PathBuf,
    entries: Vec<Entry>,
    /// id del escáner → índice en `entries` (None si lo descarta el filtro de antigüedad).
    id_to_idx: HashMap<usize, Option<usize>>,
    /// Índices de `entries` en el orden mostrado.
    view: Vec<usize>,
    cursor: usize,
    offset: usize,
    page: usize,
    touched: bool,
    sort: SortBy,
    older_than: Option<u64>,
    started: Instant,
    scan_time: Option<Duration>,
    freed: u64,
    deleted_count: usize,
    pending: usize,
    confirm: Option<Vec<usize>>,
    message: Option<(String, Instant)>,
    del_tx: Sender<DelMsg>,
}

impl App {
    fn new(root: PathBuf, sort: SortBy, older_than: Option<u64>, del_tx: Sender<DelMsg>) -> Self {
        App {
            root,
            entries: Vec::new(),
            id_to_idx: HashMap::new(),
            view: Vec::new(),
            cursor: 0,
            offset: 0,
            page: 10,
            touched: false,
            sort,
            older_than,
            started: Instant::now(),
            scan_time: None,
            freed: 0,
            deleted_count: 0,
            pending: 0,
            confirm: None,
            message: None,
            del_tx,
        }
    }

    fn event_loop(
        &mut self,
        terminal: &mut DefaultTerminal,
        scan_rx: &Receiver<ScanEvent>,
        del_rx: &Receiver<DelMsg>,
    ) -> io::Result<()> {
        let mut dirty = true;
        loop {
            dirty |= self.drain(scan_rx, del_rx);
            if dirty || self.animating() {
                terminal.draw(|f| self.draw(f))?;
                dirty = false;
            }
            if event::poll(Duration::from_millis(80))? {
                match event::read()? {
                    Event::Key(k) if k.kind == KeyEventKind::Press => {
                        if let Flow::Quit = self.on_key(k) {
                            return Ok(());
                        }
                        dirty = true;
                    }
                    Event::Resize(..) => dirty = true,
                    _ => {}
                }
            }
        }
    }

    fn animating(&self) -> bool {
        self.scan_time.is_none() || self.pending > 0 || self.message.is_some()
    }

    fn drain(&mut self, scan_rx: &Receiver<ScanEvent>, del_rx: &Receiver<DelMsg>) -> bool {
        let mut changed = false;
        for ev in scan_rx.try_iter().take(20_000) {
            self.on_scan(ev);
            changed = true;
        }
        for m in del_rx.try_iter() {
            self.on_deleted(m);
            changed = true;
        }
        if changed {
            self.resort();
        }
        if self
            .message
            .as_ref()
            .is_some_and(|(_, t)| t.elapsed() > Duration::from_secs(6))
        {
            self.message = None;
            changed = true;
        }
        changed
    }

    fn on_scan(&mut self, ev: ScanEvent) {
        match ev {
            ScanEvent::Found(f) => {
                if let Some(days) = self.older_than
                    && age_days(f.modified).is_none_or(|d| d < days)
                {
                    self.id_to_idx.insert(f.id, None);
                    return;
                }
                let display = f
                    .path
                    .strip_prefix(&self.root)
                    .unwrap_or(&f.path)
                    .display()
                    .to_string();
                let idx = self.entries.len();
                self.entries.push(Entry {
                    path: f.path,
                    display,
                    size: None,
                    modified: f.modified,
                    risky: f.risky,
                    marked: false,
                    status: Status::Present,
                });
                self.id_to_idx.insert(f.id, Some(idx));
                self.view.push(idx);
            }
            ScanEvent::Sized { id, bytes } => {
                if let Some(&Some(idx)) = self.id_to_idx.get(&id) {
                    let e = &mut self.entries[idx];
                    if e.status != Status::Deleted {
                        e.size = Some(bytes);
                    }
                }
            }
            ScanEvent::Done => self.scan_time = Some(self.started.elapsed()),
        }
    }

    fn on_deleted(&mut self, m: DelMsg) {
        self.pending -= 1;
        self.freed += m.bytes;
        let e = &mut self.entries[m.idx];
        match m.error {
            None => {
                e.status = Status::Deleted;
                e.size = Some(e.size.unwrap_or(0).max(m.bytes));
                self.deleted_count += 1;
            }
            Some(err) => {
                self.message = Some((
                    format!("No se pudo borrar {}: {err}", e.display),
                    Instant::now(),
                ));
                e.status = Status::Failed(err);
            }
        }
    }

    fn selected(&self) -> Option<usize> {
        self.view.get(self.cursor).copied()
    }

    /// Reordena manteniendo el cursor sobre la misma carpeta (para no borrar otra por error).
    fn resort(&mut self) {
        let sel = self.selected();
        let (entries, sort) = (&self.entries, self.sort);
        self.view.sort_by(|&a, &b| {
            let (a, b) = (&entries[a], &entries[b]);
            compare(
                sort,
                (a.size, a.modified, &a.path),
                (b.size, b.modified, &b.path),
            )
        });
        self.cursor = match sel {
            Some(s) if self.touched => self.view.iter().position(|&i| i == s).unwrap_or(0),
            _ => 0,
        };
    }

    fn move_by(&mut self, delta: isize) {
        let last = self.view.len().saturating_sub(1);
        self.cursor = self.cursor.saturating_add_signed(delta).min(last);
    }

    fn on_key(&mut self, key: KeyEvent) -> Flow {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Flow::Quit;
        }
        if let Some(batch) = self.confirm.take() {
            if matches!(
                key.code,
                KeyCode::Char('s' | 'S' | 'y' | 'Y') | KeyCode::Enter
            ) {
                for i in batch {
                    self.start_delete(i);
                }
            }
            return Flow::Continue;
        }
        self.touched = true;
        let page = self.page.max(1) as isize;
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Flow::Quit,
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::PageUp => self.move_by(-page),
            KeyCode::PageDown => self.move_by(page),
            KeyCode::Home | KeyCode::Char('g') => self.cursor = 0,
            KeyCode::End | KeyCode::Char('G') => self.move_by(isize::MAX),
            KeyCode::Char(' ') | KeyCode::Delete | KeyCode::Enter => self.request_delete(),
            KeyCode::Char('m' | 't') => self.toggle_mark(),
            KeyCode::Char('a') => self.toggle_all(),
            KeyCode::Char('s') => {
                self.sort = self.sort.next();
                self.resort();
            }
            KeyCode::Char('o') => self.open_selected(),
            _ => {}
        }
        Flow::Continue
    }

    fn request_delete(&mut self) {
        let marked: Vec<usize> = self
            .view
            .iter()
            .copied()
            .filter(|&i| self.entries[i].marked && self.entries[i].deletable())
            .collect();
        let batch = if !marked.is_empty() {
            marked
        } else {
            match self.selected().filter(|&i| self.entries[i].deletable()) {
                Some(i) => vec![i],
                None => return,
            }
        };
        // Una sola carpeta normal se borra al instante; lotes y rutas sensibles piden confirmación.
        if batch.len() == 1 && !self.entries[batch[0]].risky {
            self.start_delete(batch[0]);
        } else {
            self.confirm = Some(batch);
        }
    }

    fn start_delete(&mut self, idx: usize) {
        let e = &mut self.entries[idx];
        if !e.deletable() {
            return;
        }
        e.status = Status::Deleting;
        e.marked = false;
        self.pending += 1;
        let path = e.path.clone();
        let tx = self.del_tx.clone();
        rayon::spawn(move || {
            let out = delete::delete_dir(&path);
            let _ = tx.send(DelMsg {
                idx,
                bytes: out.bytes,
                error: out.error.map(|e| e.to_string()),
            });
        });
    }

    fn toggle_mark(&mut self) {
        if let Some(i) = self.selected() {
            let e = &mut self.entries[i];
            if e.deletable() {
                e.marked = !e.marked;
            }
            self.move_by(1);
        }
    }

    fn toggle_all(&mut self) {
        let all_marked = self
            .entries
            .iter()
            .filter(|e| e.deletable())
            .all(|e| e.marked);
        for e in self.entries.iter_mut().filter(|e| e.deletable()) {
            e.marked = !all_marked;
        }
    }

    fn open_selected(&mut self) {
        let Some(dir) = self
            .selected()
            .and_then(|i| self.entries[i].path.parent().map(Path::to_path_buf))
        else {
            return;
        };
        let program = if cfg!(windows) {
            "explorer"
        } else if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        let spawned = Command::new(program)
            .arg(&dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if let Err(e) = spawned {
            self.message = Some((
                format!("No se pudo abrir {}: {e}", dir.display()),
                Instant::now(),
            ));
        }
    }

    // ───────────────────────────── dibujo ─────────────────────────────

    fn draw(&mut self, f: &mut Frame) {
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(4),
            Constraint::Length(2),
        ])
        .areas(f.area());
        self.draw_header(f, header);
        self.draw_table(f, body);
        self.draw_footer(f, footer);
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let [top, stats] =
            Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);

        let status = match self.scan_time {
            None => {
                let frame =
                    SPINNER[(self.started.elapsed().as_millis() / 80) as usize % SPINNER.len()];
                Span::styled(
                    format!(
                        "{frame} buscando… {:.1}s ",
                        self.started.elapsed().as_secs_f64()
                    ),
                    Color::Yellow,
                )
            }
            Some(t) => Span::styled(
                format!("✓ escaneo en {:.2}s ", t.as_secs_f64()),
                Color::Green,
            ),
        };
        let deleting = if self.pending > 0 {
            Span::styled(format!("borrando {} · ", self.pending), Color::Yellow)
        } else {
            Span::raw("")
        };
        let right = Line::from(vec![deleting, status]).right_aligned();
        let room = (area.width as usize).saturating_sub(right.width() + 12);
        let left = Line::from(vec![
            Span::styled(
                " nmkill ",
                Style::new().fg(Color::Black).bg(Color::Cyan).bold(),
            ),
            Span::raw(" "),
            Span::styled(
                truncate_left(&self.root.display().to_string(), room),
                Color::Gray,
            ),
        ]);
        f.render_widget(Paragraph::new(left), top);
        f.render_widget(Paragraph::new(right), top);

        let releasable: u64 = self
            .entries
            .iter()
            .filter(|e| e.deletable())
            .filter_map(|e| e.size)
            .sum();
        let marked = self.entries.iter().filter(|e| e.marked).count();
        let sep = || Span::styled(" · ", Color::DarkGray);
        let mut spans = vec![
            Span::raw(" "),
            Span::styled(self.entries.len().to_string(), Modifier::BOLD),
            Span::raw(" encontradas"),
            sep(),
            Span::styled(format_size(releasable), Modifier::BOLD),
            Span::raw(" liberables"),
            sep(),
            Span::styled(
                format_size(self.freed),
                Style::new().fg(Color::Green).bold(),
            ),
            Span::raw(" liberados"),
            sep(),
            Span::raw(format!("orden: {}", self.sort.label())),
        ];
        if marked > 0 {
            spans.extend([
                sep(),
                Span::styled(format!("{marked} marcadas"), Color::Magenta),
            ]);
        }
        if let Some(d) = self.older_than {
            spans.extend([sep(), Span::raw(format!("≥ {d} días"))]);
        }
        f.render_widget(Paragraph::new(Line::from(spans)), stats);
    }

    fn draw_table(&mut self, f: &mut Frame, area: Rect) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Color::DarkGray);

        if self.view.is_empty() {
            let msg = if self.scan_time.is_none() {
                "Buscando carpetas…"
            } else {
                "No se encontró ninguna carpeta. Todo limpio ✨"
            };
            let inner = block.inner(area);
            f.render_widget(block, area);
            let y = inner.y + inner.height / 2;
            f.render_widget(
                Paragraph::new(msg).centered().fg(Color::Gray),
                Rect {
                    y,
                    height: 1,
                    ..inner
                },
            );
            return;
        }

        // Solo construimos las filas visibles: escala igual con 10 o con 100 000 resultados.
        self.page = area.height.saturating_sub(3) as usize;
        if self.cursor < self.offset {
            self.offset = self.cursor;
        } else if self.cursor >= self.offset + self.page {
            self.offset = self.cursor + 1 - self.page.max(1);
        }
        self.offset = self
            .offset
            .min(self.view.len().saturating_sub(self.page.max(1)));

        const AGE_W: u16 = 6;
        const SIZE_W: u16 = 11;
        let path_w =
            (area.width as usize).saturating_sub(2 + 2 + 1 + AGE_W as usize + SIZE_W as usize + 4);

        let end = (self.offset + self.page).min(self.view.len());
        let rows = self.view[self.offset..end]
            .iter()
            .map(|&i| self.row(&self.entries[i], path_w));

        let header = Row::new([
            Cell::from(""),
            Cell::from("Ruta"),
            Cell::from(Line::from("Mod.").right_aligned()),
            Cell::from(Line::from("Tamaño").right_aligned()),
        ])
        .style(Style::new().fg(Color::Gray).bold());

        let table = Table::new(
            rows,
            [
                Constraint::Length(1),
                Constraint::Fill(1),
                Constraint::Length(AGE_W),
                Constraint::Length(SIZE_W),
            ],
        )
        .header(header)
        .block(block)
        .row_highlight_style(Style::new().bg(SELECTED_BG))
        .highlight_symbol("› ")
        .highlight_spacing(HighlightSpacing::Always);

        let mut state = TableState::default().with_selected(Some(self.cursor - self.offset));
        f.render_stateful_widget(table, area, &mut state);

        if self.view.len() > self.page {
            let mut sb = ScrollbarState::new(self.view.len().saturating_sub(self.page))
                .position(self.offset);
            f.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(None)
                    .end_symbol(None),
                area.inner(Margin {
                    vertical: 1,
                    horizontal: 0,
                }),
                &mut sb,
            );
        }
    }

    fn row<'a>(&self, e: &'a Entry, path_w: usize) -> Row<'a> {
        let mark = if e.marked {
            Span::styled("●", Color::Magenta)
        } else {
            Span::raw(" ")
        };

        let warn = if e.risky { "⚠ " } else { "" };
        let path_text = truncate_left(&e.display, path_w.saturating_sub(warn.chars().count()));
        let (parent, name) = match path_text.rfind(['/', '\\']) {
            Some(p) => (path_text[..=p].to_string(), path_text[p + 1..].to_string()),
            None => (String::new(), path_text),
        };
        let deleted = e.status == Status::Deleted;
        let path_style = if deleted {
            Style::new().fg(Color::DarkGray).crossed_out()
        } else {
            Style::new()
        };
        let path = Line::from(vec![
            Span::styled(warn, Color::Yellow),
            Span::styled(
                parent,
                path_style.fg(if deleted {
                    Color::DarkGray
                } else {
                    Color::Gray
                }),
            ),
            Span::styled(
                name,
                if deleted {
                    path_style
                } else {
                    path_style.fg(Color::White).bold()
                },
            ),
        ]);

        let age = Line::from(format_age(e.modified))
            .right_aligned()
            .fg(Color::Gray);

        let size = match (&e.status, e.size) {
            (Status::Deleting, _) => {
                Span::styled("borrando…", Style::new().fg(Color::Yellow).italic())
            }
            (Status::Deleted, _) => Span::styled("✓ borrado", Color::Green),
            (Status::Failed(_), _) => Span::styled("✗ error", Color::Red),
            (Status::Present, None) => Span::styled("…", Color::DarkGray),
            (Status::Present, Some(b)) => Span::styled(format_size(b), size_style(b)),
        };

        Row::new([
            Cell::from(mark),
            Cell::from(path),
            Cell::from(age),
            Cell::from(Line::from(size).right_aligned()),
        ])
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let [info, keys] =
            Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);

        let info_line = if let Some((msg, _)) = &self.message {
            Line::from(Span::styled(format!(" {msg}"), Color::Red))
        } else if let Some(i) = self.selected() {
            let e = &self.entries[i];
            let mut spans = vec![Span::styled(
                format!(" {}", e.path.display()),
                Color::DarkGray,
            )];
            if e.risky {
                spans.push(Span::styled(
                    "  ⚠ ruta sensible: puede pertenecer a una aplicación instalada",
                    Color::Yellow,
                ));
            }
            Line::from(spans)
        } else {
            Line::default()
        };
        f.render_widget(Paragraph::new(info_line), info);

        let keys_line = if let Some(batch) = &self.confirm {
            let bytes: u64 = batch.iter().filter_map(|&i| self.entries[i].size).sum();
            let risky = batch.iter().any(|&i| self.entries[i].risky);
            let mut spans = vec![Span::styled(
                format!(
                    " ¿Borrar {} carpeta(s) ({})? [s/N] ",
                    batch.len(),
                    format_size(bytes)
                ),
                Style::new().fg(Color::Black).bg(Color::Yellow).bold(),
            )];
            if risky {
                spans.push(Span::styled("  ⚠ incluye rutas sensibles", Color::Yellow));
            }
            Line::from(spans)
        } else {
            let k = |key: &'static str, desc: &'static str| {
                [
                    Span::styled(key, Style::new().fg(Color::Cyan).bold()),
                    Span::styled(desc, Color::Gray),
                ]
            };
            Line::from(
                [
                    k(" ↑↓", " mover  "),
                    k("espacio", " borrar  "),
                    k("m", " marcar  "),
                    k("a", " marcar todo  "),
                    k("s", " ordenar  "),
                    k("o", " abrir carpeta  "),
                    k("q", " salir"),
                ]
                .concat(),
            )
        };
        f.render_widget(Paragraph::new(keys_line), keys);
    }
}

fn size_style(bytes: u64) -> Style {
    match bytes {
        b if b >= GIB => Style::new().fg(Color::Red).bold(),
        b if b >= 500 * MIB => Style::new().fg(Color::LightRed),
        b if b >= 100 * MIB => Style::new().fg(Color::Yellow),
        _ => Style::new(),
    }
}
