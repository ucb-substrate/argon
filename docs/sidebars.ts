import type {SidebarsConfig} from '@docusaurus/plugin-content-docs';

// Each top-level key is an independent sidebar. A page only ever shows the
// sidebar it belongs to, so the guides, the language reference, the GUI manual,
// and the tools reference read as separate books. Doc IDs are paths relative
// to docs/docs/.
const sidebars: SidebarsConfig = {
  // Each guide has a Neovim and a VS Code version, rendered from one partial in
  // docs/docs/guides/_tutorial. Add new guides to both editors here and to
  // docs/docs/guides/index.md.
  guides: [
    'guides/index',
    {
      type: 'category',
      label: 'Neovim',
      collapsed: false,
      items: [
        {
          type: 'category',
          label: 'Getting started',
          key: 'neovim-getting-started',
          collapsed: false,
          items: [
            'guides/getting-started/installation',
            'guides/getting-started/first-cell',
            'guides/getting-started/constraints',
            'guides/getting-started/hierarchy-export',
          ],
        },
        'guides/sky130-inverter',
      ],
    },
    {
      type: 'category',
      label: 'VS Code',
      collapsed: false,
      items: [
        {
          type: 'category',
          label: 'Getting started',
          key: 'vscode-getting-started',
          collapsed: false,
          items: [
            'guides/vscode/getting-started/installation',
            'guides/vscode/getting-started/first-cell',
            'guides/vscode/getting-started/constraints',
            'guides/vscode/getting-started/hierarchy-export',
          ],
        },
        'guides/vscode/sky130-inverter',
      ],
    },
  ],

  language: [
    'language/overview',
    'language/types-values',
    'language/cells-functions',
    'language/control-flow',
    'language/geometry',
    'language/constraints',
    'language/schematic',
    'language/modules-manifests',
    'language/technology',
    {
      type: 'category',
      label: 'Built-in functions',
      collapsed: false,
      link: {type: 'doc', id: 'language/builtins/index'},
      items: [
        'language/builtins/constraints',
        'language/builtins/collections',
        'language/builtins/math',
        'language/builtins/vias',
      ],
    },
    {
      type: 'category',
      label: 'Standard library',
      collapsed: false,
      link: {type: 'doc', id: 'language/std'},
      items: ['language/std-layout', 'language/std-schematic', 'language/std-symbol'],
    },
    {
      type: 'category',
      label: 'Types',
      collapsed: false,
      items: [
        'language/types/scalars',
        'language/types/rect',
        'language/types/polygon',
        'language/types/path',
        'language/types/point',
        'language/types/instance',
        'language/types/collections',
      ],
    },
  ],

  gui: [
    'gui/workspace',
    'gui/drawing',
    'gui/hierarchy-layers',
    'gui/schematic',
    'gui/symbol',
    'gui/cell-management',
    'gui/shortcuts-config',
  ],

  tools: [
    'tools/overview',
    'tools/arc',
    'tools/argone',
    'tools/argonc',
    'tools/neovim',
    'tools/vscode',
    'tools/agents',
  ],
};

export default sidebars;
