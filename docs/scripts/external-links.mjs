// Checks every external http(s) link in the built site (docs/build), which
// the Docusaurus build doesn't, and exits 1 if any fails. Run it from docs/
// after `npm run build` with `node scripts/external-links.mjs`. It isn't part
// of CI, since a third-party site's rate limits or outages would make CI flaky.
import {readFileSync, readdirSync, statSync} from 'node:fs';
import {join} from 'node:path';

const root = join(import.meta.dirname, '..', 'build');
// Hosts that refuse automated requests even when the page exists.
const SKIP = [/^https?:\/\/(www\.)?linkedin\.com/];

function htmlFiles(dir) {
  return readdirSync(dir).flatMap((name) => {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) return htmlFiles(path);
    return name.endsWith('.html') ? [path] : [];
  });
}

// Each external link, with the pages that use it.
const links = new Map();
for (const file of htmlFiles(root)) {
  const html = readFileSync(file, 'utf8');
  // Only links a reader follows; <link> tags such as preconnects aren't pages.
  for (const [, url] of html.matchAll(/<a\s[^>]*?href="(https?:\/\/[^"#]+)/g)) {
    if (SKIP.some((pattern) => pattern.test(url))) continue;
    const pages = links.get(url) ?? new Set();
    pages.add(file.slice(root.length));
    links.set(url, pages);
  }
}

async function status(url) {
  for (const method of ['HEAD', 'GET']) {
    try {
      const response = await fetch(url, {method, redirect: 'follow', signal: AbortSignal.timeout(15000)});
      if (response.ok) return response.status;
      // Some servers reject HEAD; try GET before giving up.
      if (method === 'GET') return response.status;
    } catch (error) {
      if (method === 'GET') return error.name;
    }
  }
}

const failures = [];
// A site that rate-limits (429) says nothing about the link, so it is reported
// apart from the failures.
let limited = 0;
const urls = [...links.keys()];
for (let i = 0; i < urls.length; i += 8) {
  const batch = urls.slice(i, i + 8);
  const results = await Promise.all(batch.map(status));
  batch.forEach((url, k) => {
    if (results[k] === 429) {
      limited += 1;
    } else if (!(typeof results[k] === 'number' && results[k] < 400)) {
      failures.push(`${results[k]} ${url}\n  on ${[...links.get(url)].slice(0, 3).join(', ')}`);
    }
  });
}
console.log(`${urls.length} external links: ${failures.length} failing, ${limited} rate-limited`);
for (const failure of failures) console.log(failure);
process.exit(failures.length ? 1 : 0);
