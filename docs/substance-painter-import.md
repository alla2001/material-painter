# Opening Substance 3D Painter projects

**File → Open Substance 3D Painter project (.spp)…** starts a new project from a `.spp` file, so
you can move work over from Painter. When it's done, a report lists exactly what came across and
what didn't.

## What comes across

- **The model**: every mesh in the project, with its UVs and normals.
- **Every texture set**, as a texture set of its own (the parts of the model that use its
  material), with its size (up to 4096²). Switch between them in the Texture Set panel or at the
  top of the Layers panel.
- **The final look** of each set's base color, roughness, metallic, height, opacity, emissive and normal: Painter's own
  result as of its last save, as one paint layer, *Painter result (flattened)*. This is the
  project exactly as it looked in Painter.
- **Each set's layer stack**, rebuilt in a hidden folder, *Painter layers (rebuilt)*:
  - folders and layers with their names, visibility, opacity and blend modes (Normal, Multiply
    and Subtract exactly; Overlay-like modes approximately);
  - fills of values and of **images** (the images stored in the project, with their placement:
    tiling, rotation, offset; UV or tri-planar);
  - **paint layers, from the brush strokes Painter recorded**: each stroke is replayed from its
    own camera, with its size, spacing, flow and opacity, symmetry copies included;
  - **masks, step by step in Painter's order**: their starting value, **painted strokes and
    erasers**, **polygon fills** (clicks and rectangles, by triangle, polygon, object or UV
    island), images, values, Levels and blurs, and Painter's dirt, edge wear, curvature, ambient
    occlusion, position, light and noise masks as the closest Material Painter smart masks
    (approximately). Strokes painted over a smart mask go over it, as in Painter;
  - geometry masks (a layer on some of the model's objects only).

To edit the structure instead of the flattened result, show *Painter layers (rebuilt)* and hide
*Painter result (flattened)*.

## What doesn't (the limits)

- **Procedural materials, generators and filters** (Substance `.sbsar` graphs): Material Painter
  has no Substance engine, so it can't run them. Their result is in the flattened layer; in the
  rebuilt stack, procedural material and filter layers are hidden, empty placeholders with their
  names, and smart masks are the closest Material Painter ones.
- **Brush tips**: Painter's brush tips are Substance graphs too, so strokes are replayed with
  Material Painter's round brush (the procedural round tip keeps its hardness). Painter's brush
  engine isn't copied exactly either: replayed strokes come close to Painter's, not texel for
  texel.
- **Anchor points and references** in masks, color selections and stencils: not rebuilt.
- **Channels Material Painter doesn't have**: ambient occlusion and others. Painter's specular
  level isn't read yet.
- **Baked mesh maps**: Material Painter bakes its own from the model when the project opens. If
  Painter baked from a high poly, load the same high poly in the bake window.
- **Texture sets larger than 4096²** come in at 4096².
- A channel Painter never computed before saving isn't stored in the file; open and save the
  project in Painter to refresh it.

## What is read, and what isn't

Only your own project's data is read: the model, the layer settings and the final textures you
made. The contents of Substance (`.sbsar`) materials are Adobe's assets; Material Painter never
opens them. It doesn't use any Adobe software or library to read the file.

Material Painter is not affiliated with or endorsed by Adobe. Substance 3D Painter is a trademark
of Adobe.

## Credits

Reading `.spp` files is adapted from [SubCraft](https://github.com/Aalzard/subcraft) by Aalzard
and the SubCraft contributors (MIT License): its HDF5, model, document, texture-cache and
JPEG 2000 readers and its synthetic test project, in [`src/spp`](../src/spp) with the license
text. Material Painter adds the reading of Painter's older (version 0) document format and its
own import into Material Painter's layers. How Painter's recorded strokes, polygon fills and
image placements read (screen origin, brush size, rectangle selection) follows SubCraft's
research notes (`docs/spp-research.md`), measured there on real projects.
