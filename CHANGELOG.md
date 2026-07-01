# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While pre-1.0, the minor version is bumped for breaking changes.

<!-- next-header -->

## [Unreleased] - ReleaseDate

## [0.1.0] - 2026-07-01

Initial release — the rendering-agnostic editing and layout core carved out of
the [Piki](https://github.com/roblillack/piki) editor so it can be shared
across multiple tools.

### Added

- `Editor`: the editing engine. Owns the authoritative `tdoc::Document` tree
  plus cursor/selection state and performs every mutation — typing, styling,
  structural edits, and undo/redo.
- `Renderer`: a two-phase layout/paint engine. Layout turns the document plus
  font metrics into positioned lines, runs, and table grids; paint walks the
  layout and emits drawing primitives. Owns the view state a host needs —
  viewport/scroll, cursor blink, link hover, search, and hit-testing.
- `RenderContext`: the backend trait a frontend implements to supply real text
  metrics and drawing primitives, keeping the engine independent of any UI
  toolkit.
- insta-based SVG snapshot tests run under both proportional and monospace
  metric regimes, plus layout/edit performance benchmarks.

<!-- next-url -->
[Unreleased]: https://github.com/roblillack/rutle/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/roblillack/rutle/releases/tag/v0.1.0
