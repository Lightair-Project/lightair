# lightair-moc3

Lightair's built-in runtime for `.moc3` avatar models: it reads a `.model3.json` with its
`.moc3`, pose, physics, motions, expressions and textures, drives the model's parameters, and
builds the meshes Lightair's avatar renderer draws. Pure Rust; no native SDK.

- `assets`: loading a model from disk.
- `ModelRuntime`: parameters, motions, expressions, pose fading and meshes.
- `render::common`: draw order, clipping and vertex helpers for a renderer.

Third-party code, built in from source: its origin, licence and Lightair's changes are in
`NOTICE.md` and `LICENSE`. It's excluded from the workspace, so it builds like a dependency.
