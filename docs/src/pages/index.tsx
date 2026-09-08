import {Fragment, useCallback, useEffect, useRef, useState, type ReactNode} from 'react';
import clsx from 'clsx';
import Link from '@docusaurus/Link';
import useBaseUrl from '@docusaurus/useBaseUrl';
import Layout from '@theme/Layout';
import Heading from '@theme/Heading';
import styles from './index.module.css';

const INSTALL = 'cargo install --git https://github.com/ucb-substrate/argon --locked argon';

const reasons = [
  {
    title: 'Constraints',
    body:
      'Positions and dimensions are solver variables. State how edges relate and the solver places the geometry. Change a parameter and the layout follows.',
  },
  {
    title: 'Parametric cells',
    body:
      'Cells take typed arguments and nest to form a hierarchy. You get functions, enums, loops, and modules in a Rust-like syntax, and the type checker catches mistakes before any layout is generated.',
  },
  {
    title: 'Two-way editing',
    body:
      'Neovim owns the source and the GUI renders the result. Draw on the canvas and Argon writes the code into your buffer. Edit the code and the canvas updates as you type.',
  },
];

// Icons are from Lucide (ISC license), inlined so the page has no runtime dependency.
const icons: Record<string, ReactNode> = {
  book: (
    <>
      <path d="M2 3h6a4 4 0 0 1 4 4v14a3 3 0 0 0-3-3H2z" />
      <path d="M22 3h-6a4 4 0 0 0-4 4v14a3 3 0 0 1 3-3h7z" />
    </>
  ),
  code: (
    <>
      <polyline points="16 18 22 12 16 6" />
      <polyline points="8 6 2 12 8 18" />
    </>
  ),
  window: (
    <>
      <rect x="2" y="4" width="20" height="16" rx="2" />
      <path d="M2 9h20" />
      <path d="M8 9v11" />
    </>
  ),
  terminal: (
    <>
      <polyline points="4 17 10 11 4 5" />
      <line x1="12" y1="19" x2="20" y2="19" />
    </>
  ),
};

const books = [
  {
    icon: 'book',
    title: 'Guides',
    body: 'Install Argon and build, constrain, and export a first cell.',
    to: '/guides',
    action: 'Read the guides',
  },
  {
    icon: 'code',
    title: 'Language',
    body: 'Syntax, built-in functions, the standard library, and types.',
    to: '/language/overview',
    action: 'Language reference',
  },
  {
    icon: 'window',
    title: 'GUI',
    body: 'Drawing, dimensions, hierarchy, and layers in the visual editor.',
    to: '/gui/workspace',
    action: 'GUI manual',
  },
  {
    icon: 'terminal',
    title: 'Tools',
    body: 'arc, argone, argonc, and the Neovim plugin.',
    to: '/tools/overview',
    action: 'Tools reference',
  },
];

function InstallCommand({command}: {command: string}) {
  const [copied, setCopied] = useState(false);
  const timer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  useEffect(() => () => clearTimeout(timer.current), []);

  const copy = useCallback(async () => {
    try {
      await navigator.clipboard.writeText(command);
      setCopied(true);
      clearTimeout(timer.current);
      timer.current = setTimeout(() => setCopied(false), 2000);
    } catch {
      // Clipboard access can be denied or unavailable; the text remains selectable.
    }
  }, [command]);

  return (
    <div className={styles.install}>
      <code>
        {/* One span per token so lines break only at spaces, never inside --flags. */}
        {command.split(' ').map((token, index) => (
          <Fragment key={index}>
            {index > 0 && ' '}
            <span className={styles.token}>{token}</span>
          </Fragment>
        ))}
      </code>
      <button
        type="button"
        className={clsx(styles.copy, copied && styles.copied)}
        onClick={copy}
        aria-label={copied ? 'Copied' : 'Copy install command'}
        title={copied ? 'Copied' : 'Copy'}>
        <svg
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          strokeWidth="2"
          strokeLinecap="round"
          strokeLinejoin="round"
          aria-hidden="true">
          {copied ? (
            <path d="M20 6 9 17l-5-5" />
          ) : (
            <>
              <rect width="14" height="14" x="8" y="8" rx="2" ry="2" />
              <path d="M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2" />
            </>
          )}
        </svg>
      </button>
    </div>
  );
}

function Icon({name}: {name: string}) {
  return (
    <svg
      className={styles.icon}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.75"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true">
      {icons[name]}
    </svg>
  );
}

export default function Home(): ReactNode {
  return (
    <Layout
      wrapperClassName="front-page"
      description="Argon is a constraint-based language and visual editor for integrated-circuit layout.">
      <main className={styles.main}>
        <section className={styles.hero}>
          <div className={styles.container}>
            <Heading as="h1">Argon</Heading>
            <p className={styles.tagline}>
              A language and editor for constraint-based integrated-circuit layout.
            </p>
            <div className={styles.actions}>
              <Link className={clsx(styles.btn, styles.btnPrimary)} to="/guides/getting-started/installation">
                Get started
              </Link>
              <Link className={clsx(styles.btn, styles.btnSecondary)} to="/language/overview">
                Language reference
              </Link>
            </div>
            <InstallCommand command={INSTALL} />
          </div>
        </section>

        <div className={clsx(styles.container, styles.shotWrap)}>
          <figure className={styles.shot}>
            <img
              src={useBaseUrl('/img/gui.png')}
              alt="The Argon GUI showing a differential ring oscillator in the sky130 process, with the scope tree on the left and the layer list on the right."
              width={1886}
              height={1486}
            />
            <figcaption>A differential ring oscillator for the sky130 process, written in Argon and shown in the GUI.</figcaption>
          </figure>
        </div>

        <section className={clsx(styles.container, styles.section)}>
          <Heading as="h2" className={styles.h2}>
            Why Argon?
          </Heading>
          <div className={styles.reasons}>
            {reasons.map((reason) => (
              <div key={reason.title}>
                <Heading as="h3">{reason.title}</Heading>
                <p>{reason.body}</p>
              </div>
            ))}
          </div>
        </section>

        <section className={styles.band}>
          <div className={clsx(styles.container, styles.section)}>
            <Heading as="h2" className={styles.h2}>
              Documentation
            </Heading>
            <div className={styles.cards}>
              {books.map((book) => (
                <div key={book.to} className={styles.card}>
                  <span className={styles.iconWrap}>
                    <Icon name={book.icon} />
                  </span>
                  <Heading as="h3">{book.title}</Heading>
                  <p>{book.body}</p>
                  <Link className={clsx(styles.btn, styles.btnSecondary, styles.btnSmall)} to={book.to}>
                    {book.action}
                  </Link>
                </div>
              ))}
            </div>
          </div>
        </section>

        <section className={clsx(styles.container, styles.section)}>
          <Heading as="h2" className={styles.h2}>
            Get involved
          </Heading>
          <p className={styles.involved}>
            Argon is developed on <Link href="https://github.com/ucb-substrate/argon">GitHub</Link>. Bug reports
            and pull requests are welcome. Notes for contributors live in the repository's{' '}
            <Link href="https://github.com/ucb-substrate/argon/blob/main/docs/developers.md">docs directory</Link>.
          </p>
        </section>
      </main>
    </Layout>
  );
}
