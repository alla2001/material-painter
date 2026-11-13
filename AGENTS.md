# Guide for AI coding agents

Read this before changing code in this repository. Humans: see
[CONTRIBUTING.md](CONTRIBUTING.md) too.

## What this is

Material Painter is a PBR texture painter (in the spirit of Substance
Painter) in **Rust**, with **eframe/egui 0.37** for the UI and **wgpu 30** for
all GPU work. One binary crate, edition 2024. Goals: fast, low memory, and
artist-first UI.

## Commands

```sh
cargo build                     # debug build (dependencies are optimized, see Cargo.toml)
cargo test                      # unit tests; CI runs these with RUSTFLAGS="-D warnings"
cargo test --bin material_painter -- --ignored --test-threads 1   # GPU tests, offscreen
cargo test --bin material_painter <name> -- --ignored               # one GPU test
```

GPU tests render without a window and write images to `target/bake_test/`.
Look at the images after changing rendering code; a passing test alone does
not prove the picture is right. `ui_screenshots` renders the real UI
offscreen with egui_kittest into `target/bake_test/ui/`.

## Map of the code

| Where | What |
|---|---|
| `src/main.rs` | Entry point, window options, panic hook (`crash.rs`) |
| `src/app.rs` + `src/app/` | The UI shell: workspaces (Material Paint, Material Designer, Shader Maker), docking (`dock.rs`), panels, input, actions (`Action` enum, processed once per frame), projects (`project_io.rs`), preferences, export window, welcome window |
| `src/engine.rs` | One texture set: layer stack (paint, fill, folder), masks and mask filters, GPU painting and compositing, undo hooks. Eight paint channels (`CHANNELS`): opacity (`OPACITY`) composites into the final base color's alpha; normal (`NORMAL`) holds tangent-space detail that each layer adds to the stack below (fill normal-map images projected in world space go through the baked tangent map); specular (`SPECULAR`) is the specular level of non-metals (0.5: 4%) |
| `src/app/texture_sets.rs` | Texture sets: one `Engine` per material of the mesh (`Mesh::sets`); the edited one is `PainterApp::engine`, the others are parked. Views, bakes, export and projects cover every set. Hiding a set rebuilds the picking BVH without it; highlights ride in the view globals (`SetLook`) |
| `src/cage.rs`, `src/app/cage_paint.rs` | The bake cage: projection directions (averaged over split vertices), the probe behind the live cage preview, Auto-fix, and the cage brush (`BakeSettings::cage_paint`, a front/rear multiplier per vertex) |
| `src/pen.rs` | Pen side buttons on Windows: winit reports pens as touches and drops their buttons, so a hook on the event loop (`with_msg_hook`, before winit) reads the barrel flag from `WM_POINTER*`; `view_input` pans with it like the middle button. `MP_PEN_LOG=1` logs pen flags and middle-button messages to `%TEMP%\material-painter-pen.log` |
| `src/spacemouse.rs`, `src/app/space_nav.rs` | 3D mice over USB HID (`hidapi`, pure-Rust backends): a reader thread blocks on reports and wakes the UI; `space_nav` moves the 3D camera each frame while the cap is deflected and rescans when the window regains focus |
| `src/spp_import.rs`, `src/app/spp_open.rs`, `src/app/spp_replay.rs` | Opening Substance 3D Painter projects: reading (worker thread, `src/spp` from SubCraft), rebuilding each set's stack, and replaying Painter's recorded strokes and polygon fills (`Engine::begin_replay`: stamps drawn only over the triangles they reach, no undo snapshots). Painted masks are RG: `r * smart mask + g`; imports paint over the smart mask (`Engine::set_mask_over`) |
| `src/polyfill.rs` | Polygon fill selection: triangles, quads, face loops, flat areas, parts, objects, UV islands, texture sets; lasso test |
| `src/bake.rs` | Mesh-map baking: CPU scene prep on a worker thread, then GPU passes pumped a few tiles per frame |
| `src/bake_cache.rs` | On-disk cache of baked maps, keyed by mesh, settings and the bake code itself |
| `src/viewport.rs`, `src/post.rs`, `src/raytrace.rs` | 3D and 2D view rendering, post-processing, path-traced preview |
| `src/graph/` | Material graph: node definitions (`def.rs`, WGSL files with a comment header), GPU runtime, presets; built-in nodes live in `src/graph/nodes/*.wgsl` |
| `src/shaders/*.wgsl` | Engine shaders (compositing, painting stamps, baking, views) |
| `src/presets.rs` | Smart materials, materials and mask generators on the shelf |
| `src/mesh.rs`, `src/import.rs` | Meshes, procedural primitives, OBJ/FBX import (`Mesh::load_file_with`, auto-unwrap per texture set), OBJ export, MikkTSpace tangents |
| UDIM tiles (`Mesh::split_into_udims`, `MeshSet::udim`) | On import a UV layout in UDIM tiles (each triangle inside one tile, more than tile 1001 used) is moved into 0..1 per tile and split by material and tile into sets named "Material.1001"; `udim` is saved with each set in projects, puts the UVs back in `write_obj`, and names exported files (`$udim`, else ".1001" after the name; `$set` is `base_name()`). Pref `udim_tiles` |
| `src/unwrap.rs`, `build.rs`, `vendor/xatlas` | Automatic UV unwrapping: xatlas (C++, MIT, unmodified) compiled by `build.rs` and called through its C API. Imports run on a worker thread (`src/app/mesh_import.rs`) with progress in the status bar |
| `src/graph/shading.rs`, `src/graph/nodes/shading/`, `src/app/preview_shader.rs` | Preview shaders: shading graphs compiled into one `custom_shade(c: Shade)` for the 3D view (`Shade` and the hook are in `view.wgsl`); parameters are a uniform array |
| `src/app/path_tool.rs` | Path tool: editable curves on the surface or UV map, stamped through the normal stroke pipeline |
| `src/update.rs`, `src/app/updates.rs` | Updates from GitHub releases: `Install::detect` tells the setup install (Windows, `unins000.exe` beside the program), an unpacked zip/tarball folder and the macOS bundle apart, picks the matching release package, downloads it and unpacks it beside the install. Only signed packages install: the release workflow signs each one with `tools/update-sign` (Ed25519 over "material-painter update
<file name>
<sha-256 hex>", private key in the `UPDATE_SIGNING_KEY` secret) and uploads `<package>.sig`; `SIGNING_KEY` in `update.rs` is the public half. Changing the key means older versions refuse the new releases, so keep it. `main` calls `update::finish` after the window closes: it runs the new setup (`/WAITPID`, `/RELAUNCH`, see the `[Code]` in `installer/material-painter.iss`) or renames the new files in (old ones to `.old-version`, deleted at next start) |
| `src/library.rs`, `src/app/library_ui.rs` | Asset library: Poly Haven / ambientCG catalogs and downloads, the community library (`alla2001/material-painter-community`), sharing graphs; network work on worker threads |
| `src/export.rs` | Texture export templates (engine presets) and file writing |
| `src/undo.rs` | Undo/redo of every document edit |
| `src/mcp.rs`, `src/app/mcp_tools.rs` | Optional MCP server so AI agents can drive the app (off by default) |
| `src/theme.rs`, `src/icons.rs` | UI theme (sharp corners, grey palette, blue accent) and Phosphor icon glyphs |

## Conventions (follow these)

- **No timing hacks.** Never fix ordering problems with sleeps, frame
  counting or timeouts. Use the existing patterns: worker threads that send
  results over `mpsc` channels, `queue.on_submitted_work_done` callbacks,
  `ctx.request_repaint()`, and the `Action` queue in `app.rs`.
- **Never block the UI thread** on disk, decoding or GPU readback. Heavy work
  goes to a worker thread; GPU work is split so a frame stays fast.
- **Every document edit is undoable.** Go through the engine's existing edit
  functions (they record undo entries) rather than mutating layers directly.
- **Partial updates.** Painting and compositing redraw only dirty regions; use
  `mark_dirty()` / stroke regions as the surrounding code does instead of
  forcing full recomposites.
- **Projects must stay loadable.** If you change what a project saves, keep
  reading old files (see the version handling in `project_io.rs`).
- **WGSL must compile on every backend** (DX12 via FXC/DXC, Vulkan, Metal).
  FXC rejects some things Naga accepts, for example writing to a vector with
  a dynamic index; use `select` or a local array. Test with the GPU tests.
- **egui 0.37 API.** Many examples online are for older egui, and several
  names changed (for example `ctx.global_style()` instead of `ctx.style()`,
  and `egui::Panel` for side and top panels). When in doubt, copy the pattern
  from this codebase and let the compiler's deprecation warnings guide you.
- **UI text** is short, plain and specific: say what a control does, not how
  it works. Tooltips explain the non-obvious.
- **Comments explain why**, not what. Doc comments on modules and public
  items, like the existing ones. No commented-out code.
- **Warning-free** under `-D warnings`. No `#[allow(dead_code)]` to silence
  unfinished work.
- **No new dependencies** without a clear need; say why in the PR.
- **Cross-platform:** Windows, Linux and macOS. Don't hardcode paths; data
  lives in the settings folder (`crash::data_dir()`), assets are found with
  `environment::asset_dir`.

## Common agent mistakes here

- Rewriting or reformatting unrelated code. Change only what the task needs.
- Adding a new abstraction layer for a one-off. Prefer a small function next
  to the code that uses it.
- Re-implementing something that exists: check `gpu.rs` helpers, `theme.rs`
  widgets (`theme::selectable`, `theme::pill`, `theme::card`), and the engine's
  edit functions first.
- Assuming a shader change works because it compiles: run the GPU tests and
  look at the output images.
- Forgetting the 2D (UV) view or the path tracer when changing how the 3D view
  draws something.

## Before you finish

1. `cargo build` with no warnings, and `cargo test` passes.
2. GPU tests pass if you touched rendering, baking, painting, nodes or export.
3. You tried the change in the running app.
4. The diff contains only your change.
