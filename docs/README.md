# Argon documentation

This directory holds the documentation site and the contributor notes.

## Site

The site is built with Docusaurus and lives entirely in this directory, so the
repository root stays a plain Cargo workspace.

```bash
cd docs
npm install
npm start
```

`npm run build` writes a production build to `build/`. Internal links and
anchors are checked as part of the build and a broken one fails it.

## Deployment

The site is published to GitHub Pages at <https://ucb-substrate.github.io/argon/>
by `.github/workflows/docs.yml`. The workflow builds the site on every pull
request that touches `docs/` and deploys on pushes to `main`. It needs the
repository's Pages source set to "GitHub Actions" (Settings, Pages).

The repository's Markdown link checker (lychee) skips `docs/docs`, because
the pages there link with site paths such as `/language/types/rect` that only
the site's router resolves; the Docusaurus build checks them instead.

| Path | Purpose |
| --- | --- |
| `docs/` | The published pages, one directory per book (see below) |
| `docusaurus.config.ts`, `sidebars.ts` | Site configuration and navigation |
| `src/pages/index.tsx` | The front page |
| `src/components/ApiReference.tsx` | Components used by the reference pages |
| `src/components/Editor.tsx` | Components that render a guide for one editor |
| `src/theme/prism-include-languages.js` | Prism grammar for ` ```argon ` code fences |
| `static/img/` | Favicon, social card, and the GUI screenshot on the front page |

Each book has its own sidebar. Pages are served from the site root, so the
directory name is also the URL prefix.

| Directory | Sidebar | Contents |
| --- | --- | --- |
| `docs/guides/` | Guides | `index.md` lists the guides; the Neovim versions are here and the VS Code versions under `vscode/` |
| `docs/language/` | Language | Language chapters, then `builtins/`, `std.mdx`, `std-layout.mdx`, and `types/` for the reference |
| `docs/gui/` | GUI | The visual editor |
| `docs/tools/` | Tools | `arc`, `argone`, `argonc`, the Neovim plugin, and the VS Code extension |

Sidebar entries in `sidebars.ts` are paths relative to `docs/docs/`. Links within
pages use absolute URL paths such as `/language/types/rect`.

Each guide has a Neovim and a VS Code version with the same steps. The text of
both lives once, in a partial under `docs/guides/_tutorial/`, and each version
is a short page that renders it inside `<ForEditor editor="neovim">` or
`<ForEditor editor="vscode">`. Text that differs between editors goes in
`<Neovim>` and `<VSCode>` blocks, and `<EditorName />` names the editor. Keep
headings outside those blocks so both versions have the same table of contents,
and link between guide pages with relative URLs such as `./constraints` so each
version links within itself. Partials shared across books, such as the VS Code
extension's install steps, live in `docs/_partials/`. Docusaurus doesn't publish
files or directories whose names start with `_`.

`static/img/gui-light.png` and `static/img/gui.png` show `diff_vco_top()` from
`pdks/sky130` in light and dark mode. The front page follows the site's theme.
Retake both after visible GUI changes, keeping the same dimensions and framing
and excluding the window title bar.

## Contributor notes

`developers.md`, `parser.md`, and the `gpui-*.md` files are notes for people
working on Argon itself. They are not part of the site. `figures/` holds the
source figures for the paper and README.
