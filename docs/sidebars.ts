import type {SidebarsConfig} from '@docusaurus/plugin-content-docs';

// Each top-level key is an independent sidebar. A page only ever shows the
// sidebar it belongs to, so the guides, the language reference, the GUI manual,
// and the tools reference read as separate books. Doc IDs are paths relative
// to docs/docs/.
const sidebars: SidebarsConfig = {
  // One entry per guide: a category for a multi-page guide, a single doc
  // otherwise. Add new guides here and as rows in docs/docs/guides/index.md.
  guides: [
    'guides/index',
    {
      type: 'category',
      label: 'Getting started',
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
      ],
    },
    {
      type: 'category',
      label: 'Standard library',
      collapsed: false,
      link: {type: 'doc', id: 'language/std'},
      items: ['language/std-layout', 'language/std-schematic'],
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
    'gui/cell-management',
    'gui/shortcuts-config',
  ],

  tools: [
    'tools/overview',
    'tools/arc',
    'tools/argone',
    'tools/argonc',
    'tools/neovim',
    'tools/agents',
  ],
};

export default sidebars;
