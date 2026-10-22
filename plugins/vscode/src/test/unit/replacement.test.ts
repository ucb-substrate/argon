import * as assert from 'assert';

import { minimalReplacement } from '../../replacement';

describe('minimalReplacement', () => {
  it('replaces only the changed middle', () => {
    assert.deepStrictEqual(minimalReplacement('abcdef', 'abXYef'), { start: 2, end: 4, text: 'XY' });
  });

  it('handles insertions and deletions at either end', () => {
    assert.deepStrictEqual(minimalReplacement('abc', 'abcd'), { start: 3, end: 3, text: 'd' });
    assert.deepStrictEqual(minimalReplacement('abcd', 'bcd'), { start: 0, end: 1, text: '' });
    assert.deepStrictEqual(minimalReplacement('', 'x'), { start: 0, end: 0, text: 'x' });
  });

  it('does not let the prefix and suffix overlap', () => {
    assert.deepStrictEqual(minimalReplacement('aa', 'aaa'), { start: 2, end: 2, text: 'a' });
  });
});
