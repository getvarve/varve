import { checkExpression, countSql, quoteIdent, quoteString, selectPageSql, whereSql } from './gridSql';

const pgTarget = { database: 'shop', schema: 'public', table: 'orders' };
const myTarget = { database: 'shop', schema: null, table: 'orders' };

describe('quoting', () => {
  it('doubles the identifier quote and refuses empty names', () => {
    expect(quoteIdent('postgres', 'my "t"')).toBe('"my ""t"""');
    expect(quoteIdent('mysql', 'a`b')).toBe('`a``b`');
    expect(() => quoteIdent('postgres', '')).toThrow();
    expect(() => quoteIdent('mysql', 'a\0')).toThrow();
  });

  it('writes strings safely in every MySQL mode', () => {
    expect(quoteString('postgres', "it's \\n")).toBe("'it''s \\n'");
    expect(() => quoteString('postgres', 'a\0b')).toThrow();
    expect(quoteString('mysql', "it's")).toBe("'it''s'");
    expect(quoteString('mysql', 'a\\b')).toBe("_utf8mb4 X'615C62'");
    expect(quoteString('mysql', 'é\\')).toBe("_utf8mb4 X'C3A95C'");
  });
});

describe('checkExpression', () => {
  it('accepts self-contained expressions', () => {
    for (const text of ['now()', "upper('it''s')", '(SELECT max(id) FROM t) + 1', "'a;b' || \"weird;name\"", "'--not a comment'", '1 - -1']) {
      expect(checkExpression('postgres', text)).toBeNull();
      expect(checkExpression('mysql', text)).toBeNull();
    }
    expect(checkExpression('postgres', "$$a ') -- ;$$ || $tag$x$$y$tag$")).toBeNull();
    expect(checkExpression('postgres', "E'it\\'s ) ;'")).toBeNull();
  });

  it('refuses anything that could leave its parentheses', () => {
    expect(checkExpression('postgres', '')).toMatch(/empty/);
    expect(checkExpression('postgres', '1); DROP TABLE t; (')).toMatch(/closes a parenthesis|;/);
    expect(checkExpression('postgres', '1 -- x')).toMatch(/comment/);
    expect(checkExpression('mysql', '1 # x')).toMatch(/comment/);
    expect(checkExpression('postgres', '1 /* x */')).toMatch(/comment/);
    expect(checkExpression('postgres', "'open")).toMatch(/not closed/);
    expect(checkExpression('postgres', '(1')).toMatch(/not closed/);
    expect(checkExpression('postgres', '$$x')).toMatch(/dollar/);
    expect(checkExpression('mysql', "'a\\' OR 1")).toMatch(/backslash/);
    expect(checkExpression('postgres', '1; 2')).toMatch(/';'/);
  });
});

describe('page queries', () => {
  it('builds a filtered, sorted page with a stable key order', () => {
    const sql = selectPageSql({
      dialect: 'postgres',
      target: pgTarget,
      columns: ['id', 'status'],
      filter: {
        mode: 'builder',
        conditions: [
          { column: 'status', op: '=', value: "paid'" },
          { column: 'note', op: 'like', value: '%gift%' },
          { column: 'updated_at', op: 'isNull', value: 'ignored' },
        ],
      },
      sort: [{ column: 'created_at', descending: true }],
      tiebreak: ['id'],
      limit: 200,
      offset: 400,
    });
    expect(sql).toBe(
      `SELECT "id", "status" FROM "public"."orders" WHERE "status" = 'paid''' AND CAST("note" AS text) LIKE '%gift%' AND "updated_at" IS NULL ORDER BY "created_at" DESC, "id" ASC LIMIT 200 OFFSET 400`,
    );
  });

  it('does not repeat a key column already sorted on', () => {
    const sql = selectPageSql({
      dialect: 'mysql',
      target: myTarget,
      columns: ['id'],
      filter: { mode: 'raw', sql: '' },
      sort: [{ column: 'id', descending: true }],
      tiebreak: ['id'],
      limit: 10,
      offset: 0,
    });
    expect(sql).toBe('SELECT `id` FROM `shop`.`orders` ORDER BY `id` DESC LIMIT 10 OFFSET 0');
  });

  it('wraps a raw filter in parentheses and refuses an unsafe one', () => {
    expect(whereSql('mysql', { mode: 'raw', sql: 'a = 1 OR b = 2' })).toBe('(a = 1 OR b = 2)');
    expect(() => whereSql('mysql', { mode: 'raw', sql: '1=1; DELETE FROM t' })).toThrow(/;/);
    expect(countSql('postgres', pgTarget, { mode: 'raw', sql: 'id > 5' })).toBe('SELECT COUNT(*) FROM "public"."orders" WHERE (id > 5)');
  });

  it('reads every column when they are not known yet', () => {
    expect(
      selectPageSql({ dialect: 'postgres', target: pgTarget, columns: [], filter: { mode: 'raw', sql: '' }, sort: [], tiebreak: [], limit: 5, offset: 0 }),
    ).toBe('SELECT * FROM "public"."orders" LIMIT 5 OFFSET 0');
  });

  it('refuses a bad page size', () => {
    expect(() =>
      selectPageSql({ dialect: 'postgres', target: pgTarget, columns: ['id'], filter: { mode: 'raw', sql: '' }, sort: [], tiebreak: [], limit: 0, offset: 0 }),
    ).toThrow();
  });
});
