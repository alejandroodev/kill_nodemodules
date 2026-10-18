# nmkill

Versión en Rust de [npkill](https://github.com/voidcosmos/npkill): busca todas las carpetas
`node_modules` bajo un directorio, muestra cuánto ocupan y te deja borrarlas desde una
interfaz en la terminal. Un único ejecutable, sin Node.

## Por qué es rápido

- **Escaneo paralelo** con `rayon` (work-stealing): cada subcarpeta es una tarea. Al encontrar un
  `node_modules` no entra en él; su tamaño se calcula en otra tarea, así que los resultados
  aparecen al instante y los tamaños se van rellenando.
- **Cero syscalls extra en Windows**: el tamaño de cada archivo sale de los datos de
  `FindNextFile` que ya devuelve el listado del directorio.
- **Borrado paralelo**: archivos y subcarpetas se eliminan a la vez en todos los hilos. En una
  prueba con 62 000 archivos tardó 8,4 s, frente a 46 s de `rmdir /s /q`.
- Por defecto usa 2 × núcleos hilos, porque casi todo el trabajo es E/S.
- Compilado con LTO, `codegen-units = 1` y `panic = "abort"`.

## Seguridad

- No sigue symlinks ni junctions (pnpm los usa). Si hay un enlace dentro de `node_modules`,
  borra el enlace, no lo que apunta.
- Marca con ⚠ las rutas sensibles (`AppData`, `Program Files`, `.vscode`, `.nvm`…). En la interfaz
  piden confirmación, y `--delete-all` las omite salvo con `--include-risky`.
- No entra en `.git` (configurable con `-E`).
- Quita el atributo de solo lectura cuando hace falta y reintenta si el antivirus bloquea un
  archivo un momento.
- Si sales mientras se está borrando algo, espera a que termine para no dejar carpetas a medias.

## Instalación

```bash
cargo install --path .
```

O compila y usa `target/release/nmkill.exe`:

```bash
cargo build --release
```

## Uso

```bash
nmkill                      # interfaz interactiva en el directorio actual
nmkill C:\Users\yo\proyectos
```

| Tecla | Acción |
|---|---|
| `↑` `↓` / `j` `k`, `RePág` `AvPág`, `Inicio` `Fin` | moverse |
| `Espacio` / `Supr` / `Enter` | borrar la carpeta seleccionada (o las marcadas) |
| `m` | marcar / desmarcar |
| `a` | marcar / desmarcar todo |
| `s` | cambiar el orden (tamaño → antigüedad → ruta) |
| `o` | abrir la carpeta del proyecto |
| `q` / `Esc` | salir |

### Sin interfaz (scripts)

```bash
nmkill ~/code --list                       # listar con tamaño y antigüedad
nmkill ~/code --list --json                # salida JSON
nmkill ~/code -D --older-than 90 --dry-run # qué se borraría (proyectos sin tocar en 90 días)
nmkill ~/code -D --older-than 90 -y        # borrarlo sin preguntar
```

### Opciones

| Opción | Descripción |
|---|---|
| `-t, --target <NOMBRES>` | carpetas a buscar, separadas por comas (por defecto `node_modules`; p. ej. `node_modules,.next,dist`) |
| `-E, --exclude <NOMBRES>` | carpetas en las que no entra (por defecto `.git`) |
| `--exclude-hidden` | no entra en carpetas que empiezan por `.` |
| `-s, --sort size\|age\|path` | orden inicial |
| `--older-than <DÍAS>` | solo proyectos sin cambios desde hace al menos N días |
| `-j, --threads <N>` | número de hilos |
| `-l, --list` | listar y salir |
| `--json` | salida JSON |
| `-D, --delete-all` | borrar todo lo encontrado |
| `-y, --yes` | no pedir confirmación |
| `--dry-run` | simular `--delete-all` |
| `--include-risky` | incluir rutas sensibles en `--delete-all` |

La "antigüedad" es la fecha más reciente entre los archivos de la carpeta del proyecto, sin
contar `node_modules`. Los tamaños son el tamaño real de los archivos; con pnpm, que usa hard
links a un almacén global, el espacio que se libera de verdad puede ser menor.
