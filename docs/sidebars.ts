import type {SidebarsConfig} from '@docusaurus/plugin-content-docs';

// Each top-level key is an independent sidebar. A page only ever shows the
// sidebar it belongs to, so the guides, the language reference, the GUI manual,
// and the tools reference read as separate books. Doc IDs are paths relative
// to content/.
const sidebars: SidebarsConfig = {
  // One category per guide. Add new guides as further categories here and as
  // rows in docs/guides/index.md.
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
  ],

  language: [
    'language/overview',
    'language/types-values',
    'language/cells-functions',
    'language/control-flow',
    'language/geometry',
    'language/constraints',
    'language/modules-manifests',
    'language/technology',
    {
      type: 'category',
      label: 'Built-in functions',
      collapsed: false,
      link: {type: 'doc', id: 'language/builtins/index'},
      items: [
        'language/builtins/geometry',
        'language/builtins/constraints',
        'language/builtins/hierarchy',
        'language/builtins/collections',
      ],
    },
    'language/std',
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
  ],
};

export default sidebars;
