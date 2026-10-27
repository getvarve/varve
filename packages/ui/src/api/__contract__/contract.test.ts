/**
 * The UI half of the contract check (app/src-tauri/tests/ui_contract.rs).
 * `contract.generated.ts` types every sample with `satisfies <UI type>`, so
 * `vue-tsc` fails on a shape the UI gets wrong. This test fails when a
 * sample was added or renamed without generating the file again.
 */
import { CONTRACT_IDS } from './contract.generated';

interface Sample {
  id: string;
  ts: string;
  dir: 'arg' | 'result';
}

const files = import.meta.glob<Sample[]>('./samples/[0-9][0-9]-*.json', { import: 'default', eager: true });

describe('UI contract samples', () => {
  it('are all typed in contract.generated.ts', () => {
    const ids = Object.values(files).flatMap((samples) => samples.map((s) => s.id));
    expect(ids.length).toBeGreaterThan(70);
    expect(ids.filter((id) => !CONTRACT_IDS.includes(id))).toEqual([]);
  });

  it('name a UI type for every sample', () => {
    for (const samples of Object.values(files)) for (const s of samples) expect(s.ts, s.id).not.toBe('unknown');
  });
});
