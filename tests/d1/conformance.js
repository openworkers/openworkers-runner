// D1 conformance battery, written from the Cloudflare D1 Worker API pages
// (read 2026-10-11) and the matrix in openworkers-core docs/d1.md.
// `env.DB` and `env.OTHER` are two empty databases. Each test uses its own
// tables, so the order of the tests does not matter.

// D1Database

test('prepare returns a statement without running it', async () => {
    const stmt = env.DB.prepare('SELECT * FROM table_that_does_not_exist');

    assertType(stmt.bind, 'function');
    assertType(stmt.all, 'function');
});

test('batch returns one result per statement, in order', async () => {
    await env.DB.exec('CREATE TABLE batch_order (a)');

    const insert = env.DB.prepare('INSERT INTO batch_order VALUES (?1)');
    const results = await env.DB.batch([
        insert.bind(1),
        insert.bind(2),
        env.DB.prepare('SELECT a FROM batch_order ORDER BY a'),
    ]);

    assertEqual(results.length, 3);
    assertEqual(results[0].meta.changes, 1);
    assertJSON(results[2].results, [{ a: 1 }, { a: 2 }]);
});

test('batch rolls back every statement when one fails', async () => {
    await env.DB.exec('CREATE TABLE batch_atomic (a INTEGER UNIQUE)');

    const insert = env.DB.prepare('INSERT INTO batch_atomic VALUES (?1)');

    await assertRejects(() => env.DB.batch([insert.bind(1), insert.bind(2), insert.bind(1)]), 'D1_ERROR');

    assertEqual(await env.DB.prepare('SELECT count(*) AS n FROM batch_atomic').first('n'), 0);
});

test('batch of no statement resolves to an empty array', async () => {
    assertJSON(await env.DB.batch([]), []);
});

test('batch refuses a statement of another database', async () => {
    await assertRejects(() => env.DB.batch([env.OTHER.prepare('SELECT 1')]), 'TypeError');
});

test('exec returns the count of statements and a duration', async () => {
    const result = await env.DB.exec('CREATE TABLE exec_count (a); INSERT INTO exec_count VALUES (1);');

    assertEqual(result.count, 2);
    assertType(result.duration, 'number');
});

test('exec stops at the first error and keeps the statements before it', async () => {
    await assertRejects(
        () => env.DB.exec('CREATE TABLE exec_partial (a);\nINSERT INTO exec_partial VALUES (1);\nINSERT INTO missing VALUES (1);\nINSERT INTO exec_partial VALUES (2);'),
        'D1_ERROR',
    );

    assertEqual(await env.DB.prepare('SELECT count(*) AS n FROM exec_partial').first('n'), 1);
});

test('exec accepts a statement over several lines and a ; in a string', async () => {
    const result = await env.DB.exec("CREATE TABLE exec_lines (\n  a TEXT\n);\nINSERT INTO exec_lines VALUES ('x;y');");

    assertEqual(result.count, 2);
    assertEqual(await env.DB.prepare('SELECT a FROM exec_lines').first('a'), 'x;y');
});

test('withSession is not supported', async () => {
    assertThrows(() => env.DB.withSession(), 'D1_ERROR');
});

test('dump is not supported', async () => {
    await assertRejects(() => env.DB.dump(), 'D1_ERROR');
});

// D1PreparedStatement

test('bind returns a new statement and keeps the original', async () => {
    const stmt = env.DB.prepare('SELECT ?1 AS v');
    const bound = stmt.bind(7);

    assert(bound !== stmt, 'a new statement');
    assertEqual(await bound.first('v'), 7);
    await assertRejects(() => stmt.first('v'), 'D1_RANGE_ERROR');
});

test('?NNN parameters bind by number', async () => {
    assertJSON(await env.DB.prepare('SELECT ?2 AS b, ?1 AS a').bind('a', 'b').first(), { b: 'b', a: 'a' });
});

test('? parameters bind in order', async () => {
    assertJSON(await env.DB.prepare('SELECT ? AS a, ? AS b').bind(1, 2).first(), { a: 1, b: 2 });
});

test('named parameters are refused', async () => {
    await assertRejects(() => env.DB.prepare('SELECT :name').bind(1).all(), 'D1_ERROR');
});

test('a wrong count of values is a range error', async () => {
    await assertRejects(() => env.DB.prepare('SELECT ?1, ?2').bind(1).all(), 'D1_RANGE_ERROR');
});

test('run has the D1Result shape', async () => {
    await env.DB.exec('CREATE TABLE run_shape (a)');

    const result = await env.DB.prepare('INSERT INTO run_shape VALUES (1)').run();

    assertEqual(result.success, true);
    assertJSON(result.results, []);
    assertType(result.meta.duration, 'number');
    assertEqual(result.meta.changes, 1);
    assertType(result.meta.last_row_id, 'number');
    assertEqual(result.meta.changed_db, true);
    assert(result.meta.size_after > 0, 'size_after');
});

test('all and run return the same rows', async () => {
    const stmt = env.DB.prepare('SELECT 1 AS a UNION ALL SELECT 2');

    assertJSON((await stmt.all()).results, (await stmt.run()).results);
});

test('first returns null when there is no row', async () => {
    assertEqual(await env.DB.prepare('SELECT 1 WHERE 0').first(), null);
});

test('first(column) returns null when there is no row', async () => {
    assertEqual(await env.DB.prepare('SELECT 1 AS a WHERE 0').first('a'), null);
});

test('first(column) throws for a missing column', async () => {
    await assertRejects(() => env.DB.prepare('SELECT 1 AS a').first('b'), 'D1_ERROR');
});

test('raw returns arrays in column order', async () => {
    assertJSON(await env.DB.prepare('SELECT 1 AS b, 2 AS a').raw(), [[1, 2]]);
});

test('raw with columnNames puts the names first', async () => {
    assertJSON(await env.DB.prepare('SELECT 1 AS b, 2 AS a').raw({ columnNames: true }), [['b', 'a'], [1, 2]]);
});

test('raw keeps duplicate column names', async () => {
    assertJSON(await env.DB.prepare('SELECT 1 AS id, 2 AS id').raw({ columnNames: true }), [['id', 'id'], [1, 2]]);
});

test('RETURNING rows come back from run', async () => {
    await env.DB.exec('CREATE TABLE returning_rows (id INTEGER PRIMARY KEY, a)');

    const { results } = await env.DB.prepare('INSERT INTO returning_rows (a) VALUES (?1), (?2) RETURNING id').bind('x', 'y').run();

    assertJSON(results, [{ id: 1 }, { id: 2 }]);
});

// Values

test('null, numbers and strings round trip', async () => {
    assertJSON(await env.DB.prepare('SELECT ?1 AS n, ?2 AS i, ?3 AS r, ?4 AS s').bind(null, 42, 1.5, 'text').first(), {
        n: null,
        i: 42,
        r: 1.5,
        s: 'text',
    });
});

test('a safe integer is stored as INTEGER', async () => {
    assertEqual(await env.DB.prepare('SELECT typeof(?1) AS t').bind(2 ** 40).first('t'), 'integer');
});

test('a fraction is stored as REAL', async () => {
    assertEqual(await env.DB.prepare('SELECT typeof(?1) AS t').bind(0.5).first('t'), 'real');
});

test('a boolean is stored as the integer 1 or 0', async () => {
    assertJSON(await env.DB.prepare('SELECT ?1 AS t, ?2 AS f, typeof(?1) AS type').bind(true, false).first(), {
        t: 1,
        f: 0,
        type: 'integer',
    });
});

test('an ArrayBuffer is a BLOB read back as an array of numbers', async () => {
    const result = await env.DB.prepare('SELECT ?1 AS b, typeof(?1) AS t').bind(new Uint8Array([0, 128, 255]).buffer).first();

    assertJSON(result, { b: [0, 128, 255], t: 'blob' });
});

test('a typed array is a BLOB', async () => {
    assertEqual(await env.DB.prepare('SELECT typeof(?1) AS t').bind(new Uint16Array([1, 2])).first('t'), 'blob');
});

test('undefined is a type error', async () => {
    assertThrows(() => env.DB.prepare('SELECT ?1').bind(undefined), 'D1_TYPE_ERROR');
});

test('a BigInt is a type error', async () => {
    assertThrows(() => env.DB.prepare('SELECT ?1').bind(1n), 'D1_TYPE_ERROR');
});

test('an object is a type error', async () => {
    assertThrows(() => env.DB.prepare('SELECT ?1').bind({ a: 1 }), 'D1_TYPE_ERROR');
});

// SQL

test('errors carry the D1 prefix and the SQLite code', async () => {
    await assertRejects(() => env.DB.prepare('SELECT * FROM missing_table').all(), 'D1_ERROR: no such table: missing_table: SQLITE_ERROR');
});

test('a constraint failure names the constraint', async () => {
    await env.DB.exec('CREATE TABLE unique_a (a UNIQUE); INSERT INTO unique_a VALUES (1);');
    await assertRejects(() => env.DB.prepare('INSERT INTO unique_a VALUES (1)').run(), 'UNIQUE constraint failed: unique_a.a: SQLITE_CONSTRAINT');
});

test('foreign keys are enforced', async () => {
    await env.DB.exec('CREATE TABLE fk_parent (id INTEGER PRIMARY KEY); CREATE TABLE fk_child (p REFERENCES fk_parent (id));');
    await assertRejects(() => env.DB.prepare('INSERT INTO fk_child VALUES (1)').run(), 'FOREIGN KEY constraint failed');
});

test('defer_foreign_keys defers the check to the end of a batch', async () => {
    await env.DB.exec('CREATE TABLE dfk_parent (id INTEGER PRIMARY KEY); CREATE TABLE dfk_child (p REFERENCES dfk_parent (id));');

    await env.DB.batch([
        env.DB.prepare('PRAGMA defer_foreign_keys = true'),
        env.DB.prepare('INSERT INTO dfk_child VALUES (1)'),
        env.DB.prepare('INSERT INTO dfk_parent VALUES (1)'),
    ]);

    assertEqual(await env.DB.prepare('SELECT count(*) AS n FROM dfk_child').first('n'), 1);
});

test('transaction statements are refused', async () => {
    await assertRejects(() => env.DB.prepare('BEGIN').run(), 'D1_ERROR');
    await assertRejects(() => env.DB.exec('SAVEPOINT s'), 'D1_ERROR');
});

test('ATTACH is refused', async () => {
    await assertRejects(() => env.DB.exec("ATTACH 'other.sqlite' AS other"), 'D1_ERROR');
});

test('schema pragmas are allowed', async () => {
    await env.DB.exec('CREATE TABLE pragma_info (a TEXT, b INTEGER)');

    const columns = (await env.DB.prepare('PRAGMA table_info(pragma_info)').all()).results.map((c) => c.name);

    assertJSON(columns, ['a', 'b']);
});

test('pragmas that change the journal are refused', async () => {
    await assertRejects(() => env.DB.prepare('PRAGMA journal_mode = DELETE').run(), 'D1_ERROR');
});

test('two bindings are two databases', async () => {
    await env.DB.exec('CREATE TABLE only_in_db (a)');
    await assertRejects(() => env.OTHER.prepare('SELECT * FROM only_in_db').all(), 'no such table');
});
