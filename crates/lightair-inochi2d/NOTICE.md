# lightair-inochi2d

Lightair's built-in runtime for Inochi2D and nijilive puppets. It's the `inox2d` crate of
**Inox2D**, a Rust implementation of Inochi2D by the Inochi2D project
(https://github.com/Inochi2D/inox2d), taken at commit `d4dd9dd` (version 0.3.0) under the
BSD 2-Clause licence (`LICENSE`, which keeps its copyright notice) and built into Lightair.

Changes from that commit:

- MeshGroups (Inochi2D 0.8), parameters applied at zero, a public parameter list and part
  iterator, glam 0.30.
- nijilive puppets: PathDeformers, DynamicComposites, Mask nodes, welding.
- Renamed from `inox2d` to `lightair-inochi2d`, and not published. Only the library crate is
  here: not upstream's OpenGL renderer or examples.

Ported into this crate from other BSD 2-Clause sources (the same terms as `LICENSE`, whose
conditions and disclaimer apply to them; their copyright notices are kept here):

- `src/meshgroup.rs`: MeshGroups, from Inochi2D's reference implementation
  (https://github.com/Inochi2D/inochi2d, v0.8.7, `core/nodes/meshgroup`).
  Copyright (c) 2020 Inochi2D Project.
- `src/nijilive.rs`, `src/nijilive/curve.rs`, `src/nijilive/path.rs`: deformers, curves,
  PathDeformers and welding, from nijilive (https://github.com/nijigenerate/nijilive:
  `core/nodes/node.d`, `deformable.d`, `meshgroup`, `deformer/path.d`, `deformer/curve.d`,
  `deformer/drivers/phys.d`, `drawable.d`) and its C++ port nicxlive
  (`core/nodes/curve.cpp`, `core/nodes/path_deformer.cpp`,
  `core/nodes/deformer/drivers/phys.cpp`). Copyright (c) 2020 Inochi2D Project; Copyright (c)
  2024 nijigenerate Project. Modified: translated to Rust and fitted to this crate.

`README.md` describes the crate as it's used in Lightair.
