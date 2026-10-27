import { describe, expect, it } from 'vitest';
import {
  headerCollationDatabase,
  leadingCode,
  outline,
  parseConfigEntry,
  parseQualified,
  sessionHeader,
  splitStatements,
  statementGroup,
  triggerArguments,
  withoutComments,
} from './ddlText';

describe('splitStatements', () => {
  it('splits PostgreSQL at top-level semicolons only', () => {
    const script = [
      "CREATE TABLE t (a text DEFAULT 'x;y');",
      '',
      'CREATE FUNCTION f() RETURNS int AS $body$ BEGIN RETURN 1; END; $body$ LANGUAGE plpgsql;',
      '-- note; not a statement',
      "COMMENT ON TABLE t IS E'it\\'s; fine';",
      '/* nested /* comment; */ still */ SELECT 1',
    ].join('\n');
    const parts = splitStatements(script, 'postgres');
    expect(parts.map((p) => p.line)).toEqual([1, 3, 5, 6]);
    expect(parts[1]?.text).toContain('RETURN 1; END;');
    expect(parts[2]?.text).toContain("E'it\\'s; fine'");
    expect(parts[3]?.text).toContain('SELECT 1');
  });

  it('keeps MySQL BEGIN ... END bodies, CASE expressions and END IF together', () => {
    const script = [
      'CREATE PROCEDURE p() BEGIN',
      '  IF x THEN SET y = CASE WHEN a THEN 1 ELSE 2 END; END IF;',
      '  lbl: LOOP LEAVE lbl; END LOOP lbl;',
      'END;',
      'CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW SET NEW.a = `b;c`;',
      "# a hash comment; with a semicolon",
      "SELECT 'a\\';b'",
    ].join('\n');
    const parts = splitStatements(script, 'mysql');
    expect(parts).toHaveLength(3);
    expect(parts[0]?.text.endsWith('END')).toBe(true);
    expect(parts[1]?.text).toContain('`b;c`');
    expect(parts[2]?.line).toBe(7);
  });

  it('drops blank and comment-only pieces', () => {
    expect(splitStatements('-- only a note\n;\n  ;', 'postgres')).toEqual([]);
  });
});

describe('outline and comment filtering', () => {
  const ddl = [
    '-- Column a is defined by the query of v.',
    'CREATE TABLE public.t (a int);',
    '',
    'ALTER TABLE public.t OWNER TO app;',
    '',
    'CREATE INDEX t_a ON public.t (a);',
    '',
    "COMMENT ON TABLE public.t IS 'x';",
    '',
    "COMMENT ON COLUMN public.t.a IS 'y';",
    '',
    'CREATE TRIGGER tr BEFORE UPDATE ON public.t FOR EACH ROW EXECUTE FUNCTION f();',
    '',
  ].join('\n');

  it('groups statements with their first line', () => {
    expect(outline(ddl, 'postgres')).toEqual([
      { group: 'create', label: 'Definition', line: 2, count: 1 },
      { group: 'owner', label: 'Owner', line: 4, count: 1 },
      { group: 'indexes', label: 'Indexes', line: 6, count: 1 },
      { group: 'comments', label: 'Comments', line: 8, count: 2 },
      { group: 'triggers', label: 'Triggers', line: 12, count: 1 },
    ]);
  });

  it('classifies statements behind leading comments', () => {
    expect(statementGroup('-- x\nGRANT SELECT ON t TO r')).toBe('privileges');
    expect(statementGroup('ALTER TABLE t ADD CONSTRAINT c CHECK (a > 0)')).toBe('constraints');
    expect(leadingCode('/* a */ -- b\n  SELECT 1')).toBe('SELECT 1');
  });

  it('removes COMMENT ON statements and keeps everything else', () => {
    const out = withoutComments(ddl, 'postgres');
    expect(out).not.toContain('COMMENT ON');
    expect(out).toContain('CREATE INDEX t_a');
    expect(out).toContain('CREATE TRIGGER tr');
    expect(out).not.toMatch(/\n{3,}/);
    expect(withoutComments('SELECT 1;', 'postgres')).toBe('SELECT 1;');
  });
});

describe('session header', () => {
  const ddl = '-- varve-session: sql_mode=; collation_database=utf8mb4_bin\nCREATE PROCEDURE p() BEGIN END';
  it('reads the pairs and the collation_database', () => {
    expect(sessionHeader(ddl)).toEqual([
      ['sql_mode', ''],
      ['collation_database', 'utf8mb4_bin'],
    ]);
    expect(headerCollationDatabase(ddl)).toBe('utf8mb4_bin');
    expect(sessionHeader('CREATE VIEW v AS SELECT 1')).toBeNull();
    expect(headerCollationDatabase('CREATE VIEW v AS SELECT 1')).toBeNull();
  });
});

describe('name parsers', () => {
  it('splits qualified names as PostgreSQL prints them', () => {
    expect(parseQualified('public.touch()')).toEqual({ schema: 'public', name: 'touch' });
    expect(parseQualified('"My Schema"."a""b"(integer)')).toEqual({ schema: 'My Schema', name: 'a"b' });
    expect(parseQualified('f')).toEqual({ schema: null, name: 'f' });
  });

  it('reads trigger function arguments from the definition', () => {
    expect(triggerArguments("CREATE TRIGGER t AFTER INSERT ON s.x FOR EACH ROW EXECUTE FUNCTION s.f('a', 'it''s', '3')")).toEqual(['a', "it's", '3']);
    expect(triggerArguments('CREATE TRIGGER t AFTER INSERT ON x FOR EACH ROW EXECUTE PROCEDURE "q(x)"()')).toEqual([]);
    expect(triggerArguments('no call here')).toEqual([]);
  });

  it('splits proconfig entries', () => {
    expect(parseConfigEntry('search_path=public, "my schema"')).toEqual({ name: 'search_path', values: ['public', 'my schema'] });
    expect(parseConfigEntry('work_mem=64MB')).toEqual({ name: 'work_mem', values: ['64MB'] });
  });
});
