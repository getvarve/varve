import { domId } from './domId';

describe('domId', () => {
  it('keeps safe characters and encodes the rest', () => {
    expect(domId('key:a-1')).toBe('key_3a_a-1');
  });

  it('never maps two different texts to the same id', () => {
    const texts = ['a.b:', 'a_b:', 'a b:', 'a_2e_b:', 'é', '😀', ''];
    expect(new Set(texts.map(domId)).size).toBe(texts.length);
  });
});
