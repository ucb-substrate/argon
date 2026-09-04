import type {Config} from '@docusaurus/types';
import type {Options, ThemeConfig} from '@docusaurus/preset-classic';
import {themes as prismThemes} from 'prism-react-renderer';

const config: Config = {
  title: 'Argon',
  tagline: 'Documentation for the Argon layout language and tools',
  favicon: 'img/argon-mark.svg',
  url: 'https://ucb-substrate.github.io',
  baseUrl: '/argon/',
  organizationName: 'ucb-substrate',
  projectName: 'argon',
  trailingSlash: false,
  onBrokenLinks: 'throw',
  onBrokenAnchors: 'throw',
  markdown: {
    hooks: {
      onBrokenMarkdownLinks: 'throw',
    },
  },

  i18n: {
    defaultLocale: 'en',
    locales: ['en'],
  },

  headTags: [
    {tagName: 'link', attributes: {rel: 'preconnect', href: 'https://fonts.googleapis.com'}},
    {tagName: 'link', attributes: {rel: 'preconnect', href: 'https://fonts.gstatic.com', crossorigin: 'anonymous'}},
  ],
  stylesheets: [
    // Fira Sans: body. Space Grotesk: headings, navigation, buttons.
    // JetBrains Mono: code.
    'https://fonts.googleapis.com/css2?family=Fira+Sans:ital,wght@0,400;0,500;0,600;0,700;1,400&family=Space+Grotesk:wght@500;600;700&family=JetBrains+Mono:wght@400;500;600;700&display=swap',
  ],

  presets: [
    [
      'classic',
      {
        docs: {
          // The site root is the repository's docs/ directory; the published
          // books live in docs/content/. Contributor notes at the top level of
          // docs/ are outside the content path and so are not published.
          path: 'content',
          // Docs are served at the site root so each book gets a short prefix:
          // /guides, /language, /gui, /tools.
          routeBasePath: '/',
          sidebarPath: './sidebars.ts',
          // Edit links are computed relative to the site root, which is docs/.
          editUrl: 'https://github.com/ucb-substrate/argon/edit/main/docs/',
          showLastUpdateTime: true,
        },
        blog: false,
        theme: {
          customCss: './src/css/custom.css',
        },
        sitemap: {
          changefreq: 'weekly',
          priority: 0.5,
        },
      } satisfies Options,
    ],
  ],

  themes: [
    [
      '@easyops-cn/docusaurus-search-local',
      {
        hashed: true,
        indexDocs: true,
        indexPages: true,
        indexBlog: false,
        docsRouteBasePath: '/',
        language: ['en'],
        highlightSearchTermsOnTargetPage: true,
      },
    ],
  ],

  themeConfig: {
    image: 'img/argon-social-card.svg',
    colorMode: {
      defaultMode: 'light',
      disableSwitch: false,
      respectPrefersColorScheme: true,
    },
    navbar: {
      title: 'Argon',
      logo: {
        alt: 'Argon',
        src: 'img/argon-mark.svg',
      },
      items: [
        {type: 'docSidebar', sidebarId: 'guides', label: 'Guides', position: 'left'},
        {type: 'docSidebar', sidebarId: 'language', label: 'Language', position: 'left'},
        {type: 'docSidebar', sidebarId: 'gui', label: 'GUI', position: 'left'},
        {type: 'docSidebar', sidebarId: 'tools', label: 'Tools', position: 'left'},
        {
          href: 'https://github.com/ucb-substrate/argon',
          label: 'GitHub',
          position: 'right',
        },
      ],
    },
    footer: {
      links: [
        {label: 'GitHub', href: 'https://github.com/ucb-substrate/argon'},
        {label: 'Issues', href: 'https://github.com/ucb-substrate/argon/issues'},
        {label: 'License', href: 'https://github.com/ucb-substrate/argon/blob/main/LICENSE'},
      ],
      copyright: 'Argon is distributed under the BSD-3-Clause license.',
    },
    docs: {
      sidebar: {
        // No collapse button at the bottom of the sidebar.
        hideable: false,
        autoCollapseCategories: false,
      },
    },
    tableOfContents: {
      minHeadingLevel: 2,
      maxHeadingLevel: 3,
    },
    prism: {
      theme: prismThemes.github,
      darkTheme: prismThemes.oneDark,
      // `argon` is registered in src/theme/prism-include-languages.js.
      additionalLanguages: ['toml', 'lua', 'bash'],
    },
  } satisfies ThemeConfig,
};

export default config;
