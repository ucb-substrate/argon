// Swizzled from @docusaurus/theme-classic to register a Prism grammar for
// Argon. Code fences tagged ```argon use it.
import siteConfig from '@generated/docusaurus.config';

export default function prismIncludeLanguages(PrismObject) {
  const {
    themeConfig: {prism},
  } = siteConfig;
  const {additionalLanguages} = prism;

  // Prism components mutate the Prism instance on `window`, while
  // prism-react-renderer uses its own instance. Mount it temporarily.
  const PrismBefore = globalThis.Prism;
  globalThis.Prism = PrismObject;
  additionalLanguages.forEach((lang) => {
    if (lang === 'php') {
      // eslint-disable-next-line global-require
      require('prismjs/components/prism-markup-templating.js');
    }
    // eslint-disable-next-line global-require, import/no-dynamic-require
    require(`prismjs/components/prism-${lang}`);
  });

  // Keywords mirror crates/compiler/src/parser/token.rs.
  PrismObject.languages.argon = {
    comment: [
      {pattern: /(^|[^\\])\/\*[\s\S]*?(?:\*\/|$)/, lookbehind: true, greedy: true},
      {pattern: /(^|[^\\:])\/\/.*/, lookbehind: true, greedy: true},
    ],
    string: {pattern: /"(?:\\[\s\S]|[^\\"])*"/, greedy: true},
    keyword: /\b(?:as|cell|const|else|enum|fn|for|if|in|let|match|mod|struct|use)\b/,
    boolean: /\b(?:true|false)\b/,
    'class-name': /\b[A-Z][A-Za-z0-9_]*\b/,
    function: /\b[a-z_][a-z0-9_]*(?=\s*\()/,
    number: /\b\d[\d_]*(?:\.\d*)?(?:e[+-]?\d+)?/i,
    operator: /->|=>|::|&&|\|\||[+\-*\/%<>=!]=?/,
    punctuation: /[{}[\]();,.:?]/,
  };

  delete globalThis.Prism;
  if (typeof PrismBefore !== 'undefined') {
    globalThis.Prism = PrismObject;
  }
}
