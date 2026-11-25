# lightair-avatar: notices

`src/uma.rs` evaluates `.uma` puppets by translating parts of the format's reference runtime,
Umamo (https://github.com/umamoorg/umamo, GPL-3.0), at commit `f7ef026` of its master, into Rust.
This crate is part of Lightair's desktop, which is GPL-3.0-or-later. Modified: translated from
Kotlin to Rust, reduced to what Lightair draws, and fitted to Lightair's avatar renderer
(November 2026). The parts translated:

- reading: `module/format/.../format/uma/` (`Uma.kt`, `UmaManifest.kt`, `UmaBuffers.kt`,
  `puppet/UmaPuppet.kt`, `puppet/UmaPuppetGeometry.kt`, `puppet/UmaPuppetEnums.kt`,
  `textures/UmaTextures.kt`) and `module/interop/.../interop/uma/UmaPuppetImport.kt`;
- keyform grids, channels and blend shapes: `module/runtime/.../runtime/eval/`
  (`KeyformGridSampling.kt`, `ChannelSampling.kt`, `BlendShapeSampling.kt`);
- deformers and meshes: `module/render/.../render/eval/` (`DeformerCascade.kt`,
  `DeformTransforms.kt`, `KeyformGridEval.kt`, `BlendShapeEval.kt`, `DeformPrepare.kt`,
  `RenderOrder.kt`), the render tree and visibility from `runtime/model/RenderRootDerive.kt` and
  `PuppetModel.kt`, the atlas mapping from `runtime/model/AtlasPlacement.kt`, and the colour
  maths from `render/glsl/PuppetShaders.kt`.

This code must stay GPL-3.0 and must never move into lightair-core.
