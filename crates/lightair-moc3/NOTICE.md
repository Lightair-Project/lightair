# lightair-moc3

Lightair's built-in runtime for `.moc3` avatar models. It's **Mocari**, a pure Rust runtime by
Eatgrapes (https://github.com/Eatgrapes/Mocari), taken at commit `29a4792` (version 0.4.1) under
the MIT licence (`LICENSE`, which keeps its copyright notice) and built into Lightair.

Changes from that commit:

- Renamed from `mocari` to `lightair-moc3`, and not published.
- Only the library is here: `src/`. Upstream's sample models (`assets/`), examples and
  integration tests aren't, so the unit tests that read the sample models don't run here.
- `ExpressionManager::set_weight`: the newest expression's weight (Lightair's expression weight).
- `ModelRuntime::drawable_part`: the part a drawable sits in (Lightair groups art meshes by
  part when pinning things to an avatar).

`README.md` describes the crate as it's used in Lightair.
