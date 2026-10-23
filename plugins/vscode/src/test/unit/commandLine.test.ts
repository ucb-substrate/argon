import * as assert from 'assert';

import {
  completions,
  dropTrailingBytes,
  identifierPrefix,
  subcommand,
  wordCompletions,
} from '../../commandLine';
import { globUnder } from '../../paths';

describe('subcommand', () => {
  it('reads the analyzer shape of a GUI command', () => {
    assert.deepStrictEqual(
      subcommand({ argon: { name: 'inst', argument: '', submit: false } }),
      { name: 'inst', argument: '', submit: false },
    );
    assert.deepStrictEqual(
      subcommand({ argon: { name: 'diagnostics', argument: '', submit: true } }),
      { name: 'diagnostics', argument: '', submit: true },
    );
  });

  it('has none for the message log or no command', () => {
    assert.strictEqual(subcommand('messages'), undefined);
    assert.strictEqual(subcommand(null), undefined);
    assert.strictEqual(subcommand(undefined), undefined);
  });
});

describe('completions', () => {
  const items = [{ label: 'width' }, { label: 'wire' }, { label: 'inverter', insertText: 'inverter(' }];

  it('keeps the argument before the completed identifier', () => {
    assert.deepStrictEqual(completions('child(1., wi', 2, items), [
      { label: 'width', argument: 'child(1., width' },
      { label: 'wire', argument: 'child(1., wire' },
    ]);
  });

  it('inserts the insert text when there is one', () => {
    assert.deepStrictEqual(completions('inv', 3, items), [
      { label: 'inverter', argument: 'inverter(' },
    ]);
  });

  it('clamps a prefix longer than the text', () => {
    assert.deepStrictEqual(
      completions('w', 5, items).map((candidate) => candidate.argument),
      ['width', 'wire'],
    );
  });

  it('measures the prefix in UTF-8 bytes', () => {
    assert.strictEqual(dropTrailingBytes('é(ab', 2), 'é(');
    assert.deepStrictEqual(
      completions('"é", wi', 2, items).map((candidate) => candidate.argument),
      ['"é", width', '"é", wire'],
    );
  });
});

describe('identifierPrefix', () => {
  it('is the identifier ending the text', () => {
    assert.strictEqual(identifierPrefix('child(1., wid'), 'wid');
    assert.strictEqual(identifierPrefix('child('), '');
  });
});

describe('wordCompletions', () => {
  it('completes only the first word', () => {
    assert.deepStrictEqual(wordCompletions('N', ['Nmos', 'Npn', 'Pmos']), ['Nmos', 'Npn']);
    assert.deepStrictEqual(wordCompletions('Nmos "nch"', ['Nmos']), []);
  });
});

describe('globUnder', () => {
  it('escapes glob characters in the directory', () => {
    assert.strictEqual(globUnder('/work/chip'), '/work/chip/**/*');
    assert.strictEqual(globUnder('/work/[a]{b}*?'), '/work/[[]a[]][{]b[}][*][?]/**/*');
  });
});
