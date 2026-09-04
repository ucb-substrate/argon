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

| Path | Purpose |
| --- | --- |
| `content/` | The published pages, one directory per book (see below) |
| `docusaurus.config.ts`, `sidebars.ts` | Site configuration and navigation |
| `src/pages/index.tsx` | The front page |
| `src/components/ApiReference.tsx` | Components used by the reference pages |
| `src/theme/prism-include-languages.js` | Prism grammar for ` ```argon ` code fences |
| `static/img/` | Favicon, social card, and the GUI screenshot on the front page |

Each book has its own sidebar. Pages are served from the site root, so the
directory name is also the URL prefix.

| Directory | Sidebar | Contents |
| --- | --- | --- |
| `content/guides/` | Guides | `index.md` lists the guides; each guide is a subdirectory, currently only `getting-started/` |
| `content/language/` | Language | Language chapters, then `builtins/`, `std.mdx`, and `types/` for the reference |
| `content/gui/` | GUI | The visual editor |
| `content/tools/` | Tools | `arc`, `argone`, `argonc`, and the Neovim plugin |

Sidebar entries in `sidebars.ts` are paths relative to `content/`. Links within
pages use absolute URL paths such as `/language/types/rect`.

`static/img/gui.png` is a screenshot of the GUI with `diff_vco_top()` from
`pdks/sky130` open in dark mode, cropped to remove the window title bar. Retake
it after visible GUI changes.

## Contributor notes

`developers.md`, `parser.md`, and the `gpui-*.md` files are notes for people
working on Argon itself. They are not part of the site. `figures/` holds the
source figures for the paper and README.
