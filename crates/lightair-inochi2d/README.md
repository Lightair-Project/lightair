# lightair-inochi2d

Lightair's built-in runtime for Inochi2D (`.inp`, `.inx`) and nijilive puppets: it reads a
puppet, applies its parameters, runs its physics and deformers (MeshGroups, PathDeformers,
DynamicComposites, masks, welding), and hands each part's posed mesh to a renderer through the
`InoxRenderer` trait. Lightair's avatar code (`lightair-avatar::inochi2d`) records those calls
and draws them.

Third-party code, built in from source: its origins, licences and Lightair's changes are in
`NOTICE.md` and `LICENSE`. It's excluded from the workspace, so it builds like a dependency.
